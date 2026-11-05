//! The NDJSON frame parse: one line into the fields `ipc_thread` reads from it.
//!
//! Every decision is the C's, through the same scanners [`crate::entity_json`] already ports:
//!
//! * `id` and `method` come from `json_string` (`mcp_bridge.c:6421-6422`) into the C's buffer
//!   sizes (128 and 64); a numeric `id` is therefore **absent**, which is what makes the C
//!   answer `-32600 invalid request: id required` (`:6457-6463`);
//! * `jsonrpc` is only required to be present (`:6424`, `!json_find(line, "jsonrpc")`), its
//!   value is never inspected;
//! * `params` is found with the depth-blind `json_find` and kept only when it is an object
//!   (`:6627-6646`), cut at its balanced end with braces inside strings ignored (`json_bal_end`,
//!   `:6631-6643`), exactly the span the C copies into `Req.params`;
//! * `deadlineMs` is `json_int` over that object (`:6652-6657`).

use crate::entity_json::{json_bal_end, json_find, json_int, json_string, json_string_at};

use super::{ID_BUF, METHOD_BUF};

/// The fields of one request line the IPC lane needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The string `id` after escape collapsing, `None` when the frame has no string `id`
    /// (including a numeric one) - the C's `hasId`/`idbuf` pair.
    pub id: Option<String>,
    /// The string `method`, `None` when the frame has none - the C's `hasMethod`/`method`.
    pub method: Option<String>,
    /// Whether the text contains a `jsonrpc` key at any depth (`:6424`).
    pub has_jsonrpc: bool,
    /// Where the `params` value starts in the line, for `control.cancel`'s nested lookup.
    pub params_start: Option<usize>,
    /// The raw balanced `{...}` text of `params`, `None` when absent or not an object - the
    /// span copied into `Req.params` (`:6636-6644`).
    pub params: Option<String>,
}

/// Parses one line (newline already removed) as `ipc_thread` does (`:6419-6428`).
pub(crate) fn parse(line: &[u8]) -> Frame {
    let mut idbuf = [0u8; ID_BUF];
    let id = json_string(line, b"id", &mut idbuf).then(|| cstr_string(&idbuf));

    let mut methodbuf = [0u8; METHOD_BUF];
    let method = json_string(line, b"method", &mut methodbuf).then(|| cstr_string(&methodbuf));

    let params_start = json_find(line, b"params");
    let params = match params_start {
        Some(start) if line.get(start) == Some(&b'{') => json_bal_end(line, start)
            .map(|end| String::from_utf8_lossy(&line[start..end]).into_owned()),
        _ => None,
    };

    Frame {
        id,
        method,
        has_jsonrpc: json_find(line, b"jsonrpc").is_some(),
        params_start,
        params,
    }
}

/// `json_string(pp, "id", ...)` from the `params` start of `control.cancel` (`:6570-6573`):
/// the lookup is depth-blind and runs from the `params` value to the end of the line, exactly
/// as the C's does - a member named `id` *after* `params` is found, and that is the C's
/// behaviour too.
pub(crate) fn nested_string(line: &[u8], start: usize, key: &[u8], out: &mut [u8]) -> bool {
    match json_find(&line[start..], key) {
        Some(offset) => json_string_at(line, start + offset, out),
        None => false,
    }
}

/// `json_int(params, "deadlineMs", ...)` (`:6652-6656`).
pub(crate) fn deadline_ms(params: &[u8]) -> Option<i64> {
    json_int(params, b"deadlineMs")
}

/// A C buffer as a string: up to its NUL, invalid UTF-8 replaced (the C copies bytes raw; see
/// the module docs' note in [`super`]).
fn cstr_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C's `id`/`method` reads: string values are collapsed, a numeric id is not a string
    /// id at all (the `-32600` arm), and `jsonrpc` only has to exist.
    #[test]
    fn id_and_method_follow_json_string() {
        let frame = parse(br#"{"jsonrpc":"2.0","id":"a\nb","method":"pet.text","params":{}}"#);
        // `json_string_at` (`:397-407`) collapses an escape by dropping the backslash and
        // keeping the next byte verbatim: the text `a\nb` becomes the three bytes "anb".
        assert_eq!(frame.id.as_deref(), Some("anb"));
        assert_eq!(frame.method.as_deref(), Some("pet.text"));
        assert!(frame.has_jsonrpc);
        assert_eq!(frame.params.as_deref(), Some("{}"));

        let numeric = parse(br#"{"jsonrpc":"2.0","id":7,"method":"pet.text"}"#);
        assert_eq!(numeric.id, None, "a numeric id is absent, as in the C");
        assert_eq!(numeric.method.as_deref(), Some("pet.text"));

        let no_rpc = parse(br#"{"id":"1","method":"pet.text"}"#);
        assert!(!no_rpc.has_jsonrpc);
    }

    /// The params span: the balanced object only, braces inside strings ignored, and a
    /// non-object params value is not copied (`:6627-6646`).
    #[test]
    fn params_span_is_balanced_and_string_aware() {
        let frame = parse(
            br#"{"jsonrpc":"2.0","id":"1","method":"pet.apply","params":{"a":"}","b":{"c":1}}}"#,
        );
        assert_eq!(
            frame.params.as_deref(),
            Some(r#"{"a":"}","b":{"c":1}}"#),
            "the closing brace inside the string does not end the span"
        );

        let array = parse(br#"{"jsonrpc":"2.0","id":"1","method":"m","params":[1,2]}"#);
        assert_eq!(array.params, None, "only an object params span is copied");
        assert!(array.params_start.is_some());

        let unterminated = parse(br#"{"jsonrpc":"2.0","id":"1","method":"m","params":{"a":"#);
        assert_eq!(
            unterminated.params, None,
            "an unterminated object is not copied"
        );
    }

    /// `control.cancel` looks for `id` from the params value onwards (`:6570-6573`).
    #[test]
    fn cancel_reads_the_nested_id() {
        let line = br#"{"jsonrpc":"2.0","id":"cancel-1","method":"control.cancel","params":{"id":"target"}}"#;
        let frame = parse(line);
        let start = frame.params_start.expect("params present");
        let mut buf = [0u8; ID_BUF];
        assert!(nested_string(line, start, b"id", &mut buf));
        assert_eq!(cstr_string(&buf), "target");

        let no_params = parse(br#"{"jsonrpc":"2.0","id":"cancel-1","method":"control.cancel"}"#);
        assert_eq!(no_params.params_start, None);
    }

    /// `deadlineMs` is read from the params object only (`:6652-6656`), and a negative value is
    /// readable so the caller can refuse it (`:6654`).
    #[test]
    fn deadline_is_read_from_params() {
        assert_eq!(deadline_ms(br#"{"deadlineMs":50}"#), Some(50));
        assert_eq!(deadline_ms(br#"{"deadlineMs":0}"#), Some(0));
        assert_eq!(deadline_ms(br#"{"deadlineMs":-1}"#), Some(-1));
        assert_eq!(deadline_ms(br#"{"other":1}"#), None);
    }
}
