//! The `pet.*` parameter checks, driven through the real handlers.
//!
//! The C validates every request before it touches game memory (`op_pet_open`'s path shape,
//! `op_pet_schema`'s class name, `op_pet_apply`'s text and label), so those refusals are the
//! whole answer on a host with no game - which is exactly what this test drives: the handler
//! named in `ops::HANDLERS`, a malformed `Request`, and the C's `-32602 InvalidParams` object
//! byte for byte.
//!
//! The expected frames come from `control::error_ext` - the crate's `build_error_ext`, which
//! `tests/control.rs` holds to the C - with the `data` this suite writes out by hand from the
//! `op_err` calls in `bridge/mcp_bridge_pet.inc`.

use std::time::Instant;

use cs2_bridge::control;
use cs2_bridge::ops;
use cs2_bridge::queue::Request;

/// A request with one method, one id and the given raw `params` object.
fn request(method: &str, id: &str, params: Option<&str>) -> Request {
    Request {
        id: id.to_string(),
        method: method.to_string(),
        params: params.map(std::string::ToString::to_string),
        received: Instant::now(),
        deadline: None,
        gen: 1,
    }
}

/// The reply the named handler gives; the body is looked up in the group's table, so a missing
/// registration fails here rather than answering something else.
fn call(method: &str, params: Option<&str>) -> String {
    let handler = ops::lookup(method).unwrap_or_else(|| panic!("`{method}` has no body"));
    handler(&request(method, "p1", params))
}

/// The C's `-32602 InvalidParams` frame with `reason` as the `data` object's reason.
fn invalid_params(reason: &str) -> String {
    let data = format!(r#"{{"reason":"{reason}"}}"#);
    control::error_ext(Some("p1"), -32602, "InvalidParams", &data)
}

/// `op_pet_open`'s checks (`mcp_bridge_pet.inc:467-509`), in the C's order.
#[test]
fn pet_open_refuses_every_malformed_path() {
    const SHAPE: &str = "path: relative to the addon content root (particles/...), 1..512 chars, no '..'";

    for params in [None, Some("{}"), Some(r#"{"path":""}"#), Some(r#"{"path":5}"#)] {
        assert_eq!(call("pet.open", params), invalid_params(SHAPE), "{params:?}");
    }

    // 513 characters: `json_string` copies up to 519 into `char path[520]`, so the C's own
    // `pl > 512` arm answers (`:474-475`).
    let path = format!("particles/{}.vpcf", "a".repeat(513));
    assert_eq!(
        call("pet.open", Some(&format!(r#"{{"path":"{path}"}}"#))),
        invalid_params("path: 1..512 chars")
    );

    // A backslash anywhere in the raw member: `json_string` would have collapsed it (`:476-486`).
    assert_eq!(
        call("pet.open", Some(r#"{"path":"particles/a\\b.vpcf"}"#)),
        invalid_params("path: no JSON escapes (use '/' separators)")
    );

    // A raw 0x01 byte (not an escape) is a control character (`:487-492`).
    assert_eq!(
        call("pet.open", Some("{\"path\":\"particles/a\u{1}b.vpcf\"}")),
        invalid_params("path: no control characters")
    );

    // The C's `path[0] == '\\'` arm is unreachable: the raw-member escape scan above refuses
    // any backslash first. Ported anyway (see the protocol note).
    for path in ["/particles/a.vpcf", "C:/x.vpcf"] {
        assert_eq!(
            call("pet.open", Some(&format!(r#"{{"path":"{path}"}}"#))),
            invalid_params("path: relative to the addon content root (particles/...), not absolute"),
            "{path}"
        );
    }

    for path in ["particles/x.vpcz", "particles/x.vpcfx", "a.vp", "a.vpc"] {
        assert_eq!(
            call("pet.open", Some(&format!(r#"{{"path":"{path}"}}"#))),
            invalid_params("path: a .vpcf file"),
            "{path} (below six characters, or the wrong extension)"
        );
    }

    assert_eq!(
        call("pet.open", Some(r#"{"path":"particles/../x.vpcf"}"#)),
        invalid_params("path: no '..'")
    );
}

/// `op_pet_schema`'s checks (`mcp_bridge_pet.inc:724-737`).
#[test]
fn pet_schema_refuses_every_malformed_class() {
    const REQUIRED: &str = "class (string) required, 1..127 chars, [A-Za-z0-9_]";
    for params in [None, Some("{}"), Some(r#"{"class":""}"#), Some(r#"{"class":7}"#)] {
        assert_eq!(call("pet.schema", params), invalid_params(REQUIRED), "{params:?}");
    }

    let long = "a".repeat(128);
    assert_eq!(
        call("pet.schema", Some(&format!(r#"{{"class":"{long}"}}"#))),
        invalid_params("class: 1..127 chars")
    );

    for class in ["C_OP_Foo.Bar", "C-OP", "C OP", "C_ÖP"] {
        assert_eq!(
            call("pet.schema", Some(&format!(r#"{{"class":"{class}"}}"#))),
            invalid_params("class: [A-Za-z0-9_] only"),
            "{class}"
        );
    }
}

/// `op_pet_apply`'s checks (`mcp_bridge_pet.inc:958-1013`), before the KV3 gate.
#[test]
fn pet_apply_refuses_every_malformed_request() {
    for params in [None, Some("{}"), Some(r#"{"text":5}"#)] {
        assert_eq!(
            call("pet.apply", params),
            invalid_params("text (string) required"),
            "{params:?}"
        );
    }

    // `json_text_unescape`'s own reason (`bridge/json_text.h:25`).
    assert_eq!(
        call("pet.apply", Some(r#"{"text":"a\qb"}"#)),
        invalid_params("bad_escape")
    );
    assert_eq!(
        // No closing quote anywhere after the opening one (`json_text.h:24`).
        call("pet.apply", Some(r#"{"text":"abc"#)),
        invalid_params("unterminated")
    );
    assert_eq!(
        call("pet.apply", Some(r#"{"text":""}"#)),
        invalid_params("text: 1..393216 bytes")
    );

    // One byte past `PET_TEXT_MAX` (`:981-985`).
    let huge = "a".repeat(393_217);
    assert_eq!(
        call("pet.apply", Some(&format!(r#"{{"text":"{huge}"}}"#))),
        invalid_params("text: 1..393216 bytes")
    );

    // The label's own rules (`:987-1009`): not a string, then its content.
    const LABEL: &str = "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash";
    let doc = r#"<!-- kv3 encoding:text:format:vpcf -->\n{ _class = \"CParticleSystemDefinition\" }"#;
    for params in [
        format!(r#"{{"text":"{doc}","label":7}}"#),
        format!(r#"{{"text":"{doc}","label":""}}"#),
        format!(r#"{{"text":"{doc}","label":"{}"}}"#, "L".repeat(65)),
        format!("{{\"text\":\"{doc}\",\"label\":\"a\u{7f}b\"}}"),
    ] {
        assert_eq!(call("pet.apply", Some(&params)), invalid_params(LABEL), "{params}");
    }
}
