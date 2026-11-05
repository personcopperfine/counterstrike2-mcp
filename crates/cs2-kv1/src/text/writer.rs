//! Port of upstream `KV1TextSerializer` and the parts of `KVTextSerializerBase` the KV1
//! text writer uses.
//!
//! Layout, byte for byte as upstream writes it: tabs for indentation, `\n` line endings (the
//! `StreamWriter` is given `NewLine = "\n"`), every key quoted and separated from its value by
//! a tab, and a final newline after the last `}`. A nested object writes the key on its own
//! line and its `{` at the same indentation. A quoted string keeps real newlines as they are
//! (only `"` and, with escape sequences on, `\` are escaped) — see [`super::WriteOptions`].
//!
//! A document that is a single scalar pair (`"a" "1"`) writes that one pair, which is what
//! upstream's object visitor does with a non-collection root (its `OnKeyValuePair`).
//!
//! # Differences from upstream
//!
//! * **Conditions are written back.** A pair whose [`crate::KeyValue::condition`] is set gets
//!   ` [condition]` — after the value for a scalar pair, after the key for an object pair (the
//!   two positions Valve's own files use, and the two this crate's reader attaches to the same
//!   pair, so `write`/`parse` round-trips a condition verbatim). Upstream has no conditions in
//!   its object model, so nothing is written for a document it could produce: for every input
//!   without conditions the output is byte-identical to upstream's.
//! * **Scalar formatting is Rust's `Display`.** Upstream calls
//!   `KVObject.ToString(CultureInfo.InvariantCulture)`: integers and `UInt64` come out in
//!   decimal and a `float` in .NET's shortest round-trip form, which agrees with Rust's
//!   `Display` for the ordinary range (`1234.5677`, `-0.5`) but differs where .NET switches to
//!   an exponent (`1E+20` against Rust's `100000000000000000000`) and for the two values that
//!   are not finite: Rust writes `inf`/`-inf`/`NaN`, .NET `Infinity`/`-Infinity`/`NaN`. Rust's
//!   form always parses back to the same `f32`.
//! * **Every value is stringified the way upstream would.** Upstream's `KVObject.ToString`
//!   prints a [`Value::Color`] as the decimal of the `i32` its *reader* makes of a type-6 value,
//!   a [`Value::Binary`] as `FormatBlob()` (`AB CD EF`), a [`Value::WideString`] as its text
//!   (its reader turns a type-5 value into a string) and a [`Value::Pointer`] as a decimal
//!   `i32`, all of which this writer does too — so a value read from KV1 binary and written as
//!   text is spelled the way upstream spells it, not as the wire tag. What has no upstream
//!   counterpart is only this crate's *keeping* of the type-5/type-9 tags through a round trip.
//! * Upstream also writes `""` for a null value and `"1"`/`"0"` for a boolean; this value model
//!   has neither (KV1 *text* has no nulls, and KV1 binary has no booleans).

use super::WriteOptions;
use crate::{Document, KeyValue, Value};

/// Characters upstream escapes in a quoted string (its `SearchValues.Create("\"\\")`).
const CHARS_TO_ESCAPE: [char; 2] = ['"', '\\'];

/// Serializes a document as KV1 text.
pub(super) fn write(doc: &Document, opts: &WriteOptions) -> String {
    let mut out = String::new();
    write_pair(&mut out, doc, 0, opts);
    out
}

/// Upstream `WriteKeyValuePair` / `WriteStartObject` + children + `WriteEndObject`.
fn write_pair(out: &mut String, pair: &KeyValue, indentation: usize, opts: &WriteOptions) {
    match &pair.value {
        Value::Object(object) => {
            write_indentation(out, indentation);
            write_text(out, &pair.key, opts);
            // The pair's condition sits between the key and its '{' — the position Valve's
            // files use and the reader attaches to this pair.
            write_condition(out, pair.condition.as_deref());
            out.push('\n');

            write_indentation(out, indentation);
            out.push_str("{\n");
            for child in &object.entries {
                write_pair(out, child, indentation + 1, opts);
            }
            write_indentation(out, indentation);
            out.push_str("}\n");
        }
        scalar => {
            write_indentation(out, indentation);
            write_text(out, &pair.key, opts);
            // A condition on the *document's own* pair has to sit between the key and the value:
            // written after the value it would be a conditional at document level behind the root
            // pair, which the reader rejects as data after the root object. Every other scalar
            // pair writes it after the value, the way Valve's own files do.
            let condition_after_key = indentation == 0 && pair.condition.is_some();
            if condition_after_key {
                write_condition(out, pair.condition.as_deref());
            }
            out.push('\t');
            write_text(out, &scalar_text(scalar), opts);
            if !condition_after_key {
                write_condition(out, pair.condition.as_deref());
            }
            out.push('\n');
        }
    }
}

fn write_indentation(out: &mut String, indentation: usize) {
    for _ in 0..indentation {
        out.push('\t');
    }
}

/// Upstream `KV1TextSerializer.WriteText`: always quoted; `"` is escaped unconditionally, `\`
/// only when the document is written with escape sequences (upstream `HasEscapeSequences`).
fn write_text(out: &mut String, text: &str, opts: &WriteOptions) {
    out.push('"');
    if !text.contains(CHARS_TO_ESCAPE) {
        out.push_str(text);
    } else {
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' if opts.escapes => out.push_str("\\\\"),
                other => out.push(other),
            }
        }
    }
    out.push('"');
}

/// ` [condition]` for a pair that carries one, written verbatim between the brackets it was
/// read from — a hand-built condition with a `]` or a newline in it is therefore written as it
/// is, and reads back as something else (the reader takes the text up to the first `]`).
fn write_condition(out: &mut String, condition: Option<&str>) {
    if let Some(condition) = condition {
        out.push(' ');
        out.push('[');
        out.push_str(condition);
        out.push(']');
    }
}

/// Upstream `KVObject.ToString(CultureInfo.InvariantCulture)` for every value type this model
/// has in text; see the module docs for the two deliberate deviations.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) | Value::WideString(text) => text.clone(),
        Value::Int32(value) | Value::Pointer(value) => value.to_string(),
        Value::Int64(value) => value.to_string(),
        Value::UInt64(value) => value.to_string(),
        // Rust's shortest round-trip form, which is .NET's default form for the ordinary range.
        Value::Float32(value) => value.to_string(),
        // Upstream's reader turns a type-6 colour into an `Int32` before the writer runs.
        Value::Color(bytes) => i32::from_le_bytes(*bytes).to_string(),
        // Upstream `FormatBlob`: uppercase hex bytes, space separated, empty for an empty blob.
        Value::Binary(bytes) => bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
        // Unreachable for a text document: a collection is written as a block by `write_pair`.
        Value::Object(_) => String::new(),
    }
}
