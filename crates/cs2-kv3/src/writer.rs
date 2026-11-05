//! Writer: [`Document`] -> KV3 text in Valve's own layout.
//!
//! The rules (all derived from the shipped `.vmdl` / `.vdata` / `.vcompmat` / `.econitem`
//! files, see the crate docs and `tests/real_files.rs`):
//!
//! * tabs, CRLF, and **no** newline after the final `}` — Valve's tools never emit one;
//! * a block value (object, or an array that is not inline) is introduced by `key = ` with a
//!   trailing space, then the `{` / `[` on the next line at the key's own indent;
//! * inside a multi-line array every element — the last one included — ends with `,`;
//! * an array of 2 or 3 numbers is inline (`[ 0.0, 0.0, 0.0 ]`), an empty one is `[  ]`;
//! * a multi-line array of numbers packs 4 per line; any other array gets one element per line;
//! * a blob reproduces Valve's layout: `#[` and the first row at column 0, the remaining rows one
//!   level deeper, the closing `]` at the key's indent.

use crate::{is_plain_key, Document, Key, Value};

/// Line ending of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newline {
    Lf,
    Crlf,
}

impl Newline {
    pub fn as_str(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::Crlf => "\r\n",
        }
    }
}

/// Output layout: line ending plus whether the file ends with a newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub newline: Newline,
    pub final_newline: bool,
}

impl Style {
    /// What Valve's tools write, and what [`write`] uses: CRLF, no final newline (93 of the 95
    /// shipped KV3 files end this way; the two that do not contain comments).
    pub const VALVE: Style = Style { newline: Newline::Crlf, final_newline: false };

    /// `\n` line endings and a trailing newline (what the `ValveResourceFormat` test inputs use).
    pub const LF: Style = Style { newline: Newline::Lf, final_newline: true };
}

/// Write a document in Valve's layout (CRLF, no final newline).
pub fn write(doc: &Document) -> String {
    write_styled(doc, Style::VALVE)
}

/// Write a document with an explicit [`Style`].
pub fn write_styled(doc: &Document, style: Style) -> String {
    let nl = style.newline.as_str();
    let mut out = String::new();
    out.push_str(&doc.header.raw);
    out.push_str(nl);
    out.push('{');
    out.push_str(nl);
    for (key, value) in &doc.root.entries {
        write_entry(key, value, 1, nl, &mut out);
    }
    out.push('}');
    if style.final_newline {
        out.push_str(nl);
    }
    out
}

/// Render one value on its own lines at `indent` (debug helper; `\n` line endings, no trailing
/// newline). Scalars come back prefixed with their indent, blocks start with their brace.
pub fn write_value(value: &Value, indent: usize) -> String {
    let mut out = String::new();
    match value {
        value if is_flat(value) => {
            out.push_str(&"\t".repeat(indent));
            write_flat(value, &mut out);
        }
        value => write_block(value, indent, "\n", &mut out),
    }
    out
}

/// `key = value` at `indent`, terminated with `nl`.
fn write_entry(key: &Key, value: &Value, indent: usize, nl: &str, out: &mut String) {
    out.push_str(&"\t".repeat(indent));
    write_key(key, out);
    out.push_str(" = ");
    if is_flat(value) {
        write_flat(value, out);
        out.push_str(nl);
    } else {
        out.push_str(nl);
        write_block(value, indent, nl, out);
        out.push_str(nl);
    }
}

/// A block value starting on its own line at `indent`: `{`/`[`/`#[`, children, closer.
fn write_block(value: &Value, indent: usize, nl: &str, out: &mut String) {
    let pad = "\t".repeat(indent);
    match value {
        Value::Object(object) if object.is_empty() => {
            out.push_str(&pad);
            out.push_str("{  }");
        }
        Value::Array(items) if items.is_empty() => {
            out.push_str(&pad);
            out.push_str("[  ]");
        }
        Value::Object(object) => {
            out.push_str(&pad);
            out.push('{');
            out.push_str(nl);
            for (key, value) in &object.entries {
                write_entry(key, value, indent + 1, nl, out);
            }
            out.push_str(&pad);
            out.push('}');
        }
        Value::Array(items) => {
            out.push_str(&pad);
            out.push('[');
            out.push_str(nl);
            let inner = "\t".repeat(indent + 1);
            if items.iter().all(|item| matches!(item, Value::Number(_))) {
                for chunk in items.chunks(4) {
                    out.push_str(&inner);
                    for (i, item) in chunk.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_flat(item, out);
                    }
                    out.push(',');
                    out.push_str(nl);
                }
            } else {
                for item in items {
                    if is_flat(item) {
                        out.push_str(&inner);
                        write_flat(item, out);
                    } else {
                        write_block(item, indent + 1, nl, out);
                    }
                    out.push(',');
                    out.push_str(nl);
                }
            }
            out.push_str(&pad);
            out.push(']');
        }
        Value::Blob(bytes) => {
            // Valve's own layout: `#[` and the first row at column 0, the rest one level in.
            out.push_str("#[");
            out.push_str(nl);
            for (i, chunk) in bytes.chunks(32).enumerate() {
                if i > 0 {
                    out.push_str(&"\t".repeat(indent + 1));
                }
                for (j, byte) in chunk.iter().enumerate() {
                    if j > 0 {
                        out.push(' ');
                    }
                    out.push_str(&format!("{byte:02X}"));
                }
                out.push_str(nl);
            }
            out.push_str(&pad);
            out.push(']');
        }
        Value::Flagged { flag, value } => {
            // A flag whose payload is a block (`flag:` then `{`/`[` on the next line).
            out.push_str(&pad);
            out.push_str(flag);
            out.push(':');
            out.push_str(nl);
            write_block(value, indent, nl, out);
        }
        scalar => {
            out.push_str(&pad);
            write_flat(scalar, out);
        }
    }
}

/// Can this value be written on one line after `key = `? Numbers, strings, bools, `null`, a
/// flag around a scalar, and arrays of 2–3 numbers (empty objects/arrays too).
fn is_flat(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => true,
        Value::Flagged { value, .. } => is_flat(value),
        Value::Array(items) => items.is_empty() || is_inline_array(items),
        Value::Object(object) => object.is_empty(),
        Value::Blob(_) => false,
    }
}

fn is_inline_array(items: &[Value]) -> bool {
    (2..=3).contains(&items.len()) && items.iter().all(|item| matches!(item, Value::Number(_)))
}

fn write_flat(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.text),
        Value::String(text) => write_quoted(text, out),
        Value::Flagged { flag, value } => {
            out.push_str(flag);
            out.push(':');
            write_flat(value, out);
        }
        Value::Array(items) if items.is_empty() => out.push_str("[  ]"),
        Value::Array(items) => {
            out.push_str("[ ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_flat(item, out);
            }
            out.push_str(" ]");
        }
        Value::Object(object) if object.is_empty() => out.push_str("{  }"),
        // Not flat; `is_flat` gates every caller, so this is unreachable.
        Value::Object(_) | Value::Blob(_) => {}
    }
}

fn write_key(key: &Key, out: &mut String) {
    if key.quoted || !is_plain_key(&key.text) {
        write_quoted(&key.text, out);
    } else {
        out.push_str(&key.text);
    }
}

/// `"…"` with `\" \\ \n \t \r` escapes (never `"""`: shipped files embed newlines as `\n`).
fn write_quoted(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
}
