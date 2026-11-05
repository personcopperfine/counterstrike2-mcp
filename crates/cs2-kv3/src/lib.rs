//! `cs2-kv3` — the KV3 text format (Valve KeyValues3, `.vmdl` / `.vdata` / `.vcompmat` …).
//!
//! Two halves and nothing else: [`parse`] turns Valve-formatted text into a [`Document`]
//! (the `<!-- kv3 … -->` header plus one [`Object`]), [`write`] turns it back into text **in
//! Valve's own layout**, byte for byte for files Valve's tools produced.
//!
//! ```no_run
//! let text = std::fs::read_to_string("models/mcp_probe/glass_probe.vmdl").unwrap();
//! let doc = cs2_kv3::parse(&text).unwrap();
//! assert_eq!(cs2_kv3::write(&doc), text);      // byte-identical round trip
//! let root = doc.root.get("rootNode").and_then(|v| v.as_object()).unwrap();
//! assert_eq!(root.get("_class").and_then(|v| v.as_str()), Some("RootNode"));
//! ```
//!
//! # Grammar (upstream: ValveKeyValue's KV3 text reader, commit 28ca0e5)
//!
//! * A header comment is **required** and must come first (leading whitespace/comments before
//!   it are fine): `<!-- kv3 encoding:text:version{e21c7f3c-…} format:modeldoc36:version{972dada4-…}
//!   -->`. It is a *token* stream, not a line — whitespace, comments and even newlines may sit
//!   between its tokens, keywords are case-insensitive, the encoding GUID is checked against
//!   the text-encoding GUID and a `generic` format GUID against the generic one. [`Header`]
//!   keeps the consumed span verbatim in [`Header::raw`] and exposes `encoding` / `format`;
//!   [`Header::validate_strict`] is the strict upstream reader (the next bullet's union keeps
//!   `version{x}` spellings and 686975d one-liners accepted, which upstream refuses).
//! * Body: exactly one object, `{ key = value … }`. **Entry and element commas are optional**,
//!   anywhere the 686975d reader or upstream accepted one: `{ d = 2, e = 3, }`, `f = null,`
//!   and even a leading `{ , a = 1 }` / `[ , 1 ]` (686975d; upstream wants a comma only between
//!   array elements), and `[ 1 2 ]` (686975d said so in its docs; upstream needs the commas).
//! * Keys: a bare identifier `[A-Za-z_.][A-Za-z0-9_.]*` (a leading `.` is one) or a quoted
//!   string (`"…"`, `'…'`, or a `"""` multi-line string — written back quoted).
//! * Values: `null`, `true`, `false` (case-insensitive), a number, a string (`"…"` or `'…'`,
//!   the cs2-kv3 escape table: `\n`, `\t`, `\r`, `\"`, `\\` are real escapes, any other `\X`
//!   keeps the backslash and the character, and a real newline is allowed and kept; the escape
//!   table is the same inside `'…'`, so `'it\'s'` reads as `it\'s` — the backslash stays, as
//!   for any unknown escape), a raw multi-line string (`"""`: it closes at the **first**
//!   `"""`, a lone CR reads as a newline, one newline is dropped at each end — 686975d's
//!   rule; only a file that fails to parse that way is read again with upstream's, where the
//!   opener needs a newline, the closer sits on a line of its own and `\"""` un-escapes), a
//!   flagged value (`resource_name:"x"` — any identifier as flag name, then `:` or `|`, then
//!   the value; a chain nests (`a:b:v` is `a` flagging `b:v`, as 686975d read it — upstream
//!   keeps only the last flag), and a comment may sit between the name and the `:`; upstream
//!   only knows seven canonical flag names and refuses others, which 686975d never did), a blob (`#[ 41 42 … ]`, hex bytes, with
//!   686975d's nibble accumulation — `#[ 0102 ]` is two bytes — and upstream's comments
//!   between them), an array (see commas above) or a nested object.
//! * Unquoted tokens end where 686975d ended them: a name is `[A-Za-z_][A-Za-z0-9_.]*`, any
//!   other token is a run of `[A-Za-z0-9_.+-]`, so `1// c`, `true#[01]`, `[3/*c*/]` and
//!   `[null-5]` split at the `//`, `#`, `/*` and `-`. Upstream swallows those into one token.
//! * Numbers (upstream `ParseValue`; the token is the whole run described above, so `1-2` and
//!   `1_0` are refused; [`Number::parse`] accepts a wider character set than 686975d's
//!   `-?digits(.digits)?(e±digits)?`, which only ever adds accepted spellings): `-i64`, `+u64` (`007`, `+1337`, `18446744073709551615`),
//!   any `+-0-9.eE`-only token that parses as f64 (`.5`, `5.`, `1e5`, `1.23456E+2`; overflow
//!   reads as infinity), and the literals `inf` / `+inf` / `-inf` / `nan` (case-insensitive).
//! * `// line` and `/* block */` comments are accepted anywhere between tokens. A `//` comment
//!   ends at the newline even after a trailing `\` (upstream continues it; Windows paths end
//!   comments with a backslash).
//!
//! # The union
//!
//! The reader accepts every input the pre-conformance reader (commit 686975d) accepted, reading
//! it exactly as 686975d did, **plus** the upstream inputs 686975d refused (single quotes, `|`
//! flag separators, real newlines inside quoted strings, the `+5`/`.5`/`5.` number forms,
//! `inf`/`nan` literals, case-insensitive header keywords, comment-tolerant blobs, upstream's
//! `"""` forms in a file that fails with 686975d's rule). Wherever both accept an input and
//! would read it differently, 686975d wins because production already reads its output.
//! Upstream behaviour given up for that reason: `//` comment continuation after `\`, tokens
//! that swallow `/`, `#` and `-`, the strict `"""` rules whenever 686975d's reading is
//! possible, and the strict header (see [`Header::parse_strict`]). The writer is unchanged
//! from 686975d (byte-identical); only the reader widened. Checked by a differential fuzzer
//! (old reader vs this one, not part of the repo) and `tests/compat_686975d.rs`.
//!
//! # Writer rules (Valve's layout, verified against shipped content)
//!
//! Everything below was derived from the 95 `<!-- kv3 … -->` files under
//! `content/` of a real CS2 workshop install, 93 of which round-trip **byte-identically**
//! (see `tests/real_files.rs`; the two that do not contain comments — see limitations):
//!
//! * tabs for indentation, CRLF line endings, no newline after the final `}` (93/95 files);
//! * `key = value` for scalars; for an object / a non-inline array the key line ends with
//!   `" = "` (**trailing space**) and the `{` / `[` opens on the next line at the same indent;
//! * array elements that are objects/arrays are each followed by `,`; the closer is not;
//! * an array of **2 or 3 numbers** is inline (`exception_list = [  ]` when empty,
//!   `angle = [ 0.0, 0.0, 0.0 ]`). Every other array is multi-line at one indent deeper; a
//!   multi-line array of numbers packs **4 numbers per line**, anything else gets one element
//!   per line, and each of those lines ends with `,` — including the last one;
//! * empty objects are written as `{  }` (two spaces, symmetric with `[  ]`);
//! * strings are always written with escapes (never as `"""`): 28 shipped files embed newlines
//!   as `\n` inside quoted strings, so that is what the writer has to produce;
//! * blobs reproduce the layout Valve's tools emit: `key = ` + newline, then `#[` at **column 0**,
//!   the first 32-byte row also at column 0, the following rows one indent deeper than the key,
//!   and the closing `]` back at the key's indent (verified on all 20 shipped blob files — see
//!   `fixtures/flagged_blob.kv3`). This is a quirk of Valve's writer, faithfully reproduced;
//! * a quoted key that did not need quoting stays quoted ([`Object::insert`] decides for new keys).
//!
//! # Limitations (deliberate, documented)
//!
//! * **The root must be an object.** Upstream accepts any root value (`null`, a string, a
//!   number, a blob, an array, a flagged value); `Document.root` is an `Object` and is frozen
//!   that way — hammer-mcp depends on it, so `parse` refuses other roots with a clear error.
//!   Making `root` a [`Value`] (upstream's `KVDocument` holds a value) is the recorded change
//!   request in `workflows/`; it would unlock ValveKeyValue's `RootTypesTestCase`.
//! * **Comments are not preserved.** They are skipped by the parser, so a file containing them
//!   does not round-trip byte-identically: `content/csgo/scripts/propdata.vdata` (229 line
//!   comments plus whitespace-only and tab-padded lines) and
//!   `content/csgo_addons/addon_template/soundevents/soundevents_addon.vsndevts` (comment header)
//!   are the only two of the 95 shipped files that differ, and only in their comment/blank lines.
//! * **Line endings and the final newline are normalised** to Valve's canonical output (CRLF,
//!   no final newline). [`write_styled`] writes LF and/or a final newline instead. A file whose
//!   body uses spaces for indentation (the ValveResourceFormat test inputs) or no indentation at
//!   all therefore also does not round-trip byte-identically — it parses and re-serialises
//!   semantically intact.
//! * **Escapes now round-trip symmetrically.** The reader's table is the cs2-kv3 one
//!   (`\n`, `\t`, `\r`, `\"`, `\\` real; other `\X` keeps the backslash), and the writer emits
//!   exactly those, so a escaped CR survives a write+parse round trip again. Upstream's reader
//!   maps only `\n` and `\t` and *drops* the backslash for other `\X` — that would have turned
//!   the writer's `\r` into "r", which is what round 1 shipped and the review sent back.
//! * **A bare `+` value is normalised to the lexeme `0`.** Upstream reads it as the integer 0
//!   too; `[Value::Number::parse]` still accepts the `+` lexeme itself.
//! * `encoding:` other than `text` is rejected ([`parse`] only reads text KV3).
//! * `"""` raw strings are read (content normalised to `\n`, one leading/trailing newline
//!   dropped) but written back escaped.
//! * [`json`] conversions go through `serde_json`, whose maps are ordered alphabetically: object
//!   key order survives `parse`/`write`, but not a `to_json`/`from_json` round trip.
//!
//! Nothing here knows about models, files or the bridge: `cs2-kv3` is `serde_json` and
//! `std` only, no `unsafe`.

#![forbid(unsafe_code)]

mod header;
mod json;
mod lexer;
mod parser;
mod writer;

pub use json::{from_json, to_json, ConvError};
pub use parser::parse;
pub use writer::{write, write_styled, write_value, Newline, Style};

use std::fmt;

/// How deep [`parse`] goes before it refuses a file: the number of open nesting levels, where a
/// level is an object (`{`), an array (`[`) or a `flag:` in a chain, and the top-level object
/// counts as level 1.
///
/// 256 matches `KV3_CHECK_DEPTH_CAP` in `kv3_check.h`, which counts the same way over the
/// raw token stream (the root `{` is already one level). The deepest real content file nests 19
/// levels, so the cap only ever rejects hostile input — it exists because the parser recurses, and
/// an MCP server thread has a 2 MB stack.
///
/// **Lower both caps together** after the live depth probe (pet must survive what the bridge
/// lets through).
pub const MAX_DEPTH: usize = 256;

/// A number that remembers the exact lexeme it was read or built with.
///
/// `Number` never re-formats a value it read: `0.0` stays `0.0`, `74.56868` stays `74.56868`
/// (`-0.047012` is one token, not a minus sign and a float). Use [`Number::parse`] (or the
/// constructors) to obtain one, [`Number::as_f64`] / [`Number::as_i64`] to read it back.
///
/// One deliberate exception to lexeme preservation: a bare `+` value (upstream reads it as 0)
/// is normalised to the lexeme `0`, since `+` has no value of its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Number {
    /// The validated lexeme: see [`is_number_lexeme`] for the accepted forms.
    pub text: String,
}

impl Number {
    /// A `Number` from an integer.
    pub fn from_i64(i: i64) -> Self {
        Self { text: i.to_string() }
    }

    /// A `Number` from a float: Rust's shortest representation, with `.0` appended when the
    /// result is integral (`1.0`, not `1`) so the value stays a float when it is read back.
    ///
    /// Not a number (NaN, ±inf) has no KV3 spelling and becomes `0.0` (the writer's output is
    /// Valve's own layout and pinned byte-for-byte, so `inf`/`nan` from Rust values is out of
    /// scope; the reader still accepts the `inf`/`nan` *literals* upstream accepts).
    pub fn from_f64(f: f64) -> Self {
        if !f.is_finite() {
            return Self { text: "0.0".to_string() };
        }
        let mut text = format!("{f}");
        if !text.contains(['.', 'e', 'E']) {
            text.push_str(".0");
        }
        Self { text }
    }

    /// Validate a lexeme (see [`is_number_lexeme`] for the accepted forms). `None` when it is
    /// not a KV3 number.
    pub fn parse(text: &str) -> Option<Number> {
        if !is_number_lexeme(text) {
            return None;
        }
        Some(Number { text: text.to_string() })
    }

    /// The value as `f64`, when the lexeme is in range.
    pub fn as_f64(&self) -> Option<f64> {
        self.text.parse().ok()
    }

    /// The value as `i64`, when the lexeme is an integer in range (`1.5` and `1e3` are `None`).
    pub fn as_i64(&self) -> Option<i64> {
        self.text.parse().ok()
    }

    /// True when the lexeme is not a plain integer spelling (`1.0`, `1e3`, and the `inf`/`nan`
    /// literals, which upstream types as `FloatingPoint64`).
    pub fn is_float(&self) -> bool {
        self.text.contains(['.', 'e', 'E'])
            || matches!(self.text.to_ascii_lowercase().as_str(), "nan" | "inf" | "+inf" | "-inf")
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<i64> for Number {
    fn from(value: i64) -> Self {
        Number::from_i64(value)
    }
}

impl From<f64> for Number {
    fn from(value: f64) -> Self {
        Number::from_f64(value)
    }
}

/// One KV3 value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    /// A flagged value (`resource_name:"x"`, `panorama:"…"`, `soundevent:"…"`).
    Flagged { flag: String, value: Box<Value> },
    /// A `#[ … ]` byte blob.
    Blob(Vec<u8>),
    Array(Vec<Value>),
    Object(Object),
}

impl Value {
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The number's value as `f64`, when this is a number whose lexeme is in range.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(number) => number.as_f64(),
            _ => None,
        }
    }

    /// The number's value as `i64`, when this is a number and its lexeme is an integer in range
    /// (`1.5`, `inf` and the u64-range lexemes are `None`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(number) => number.as_i64(),
            _ => None,
        }
    }

    /// True for anything written on one line: `null`, a bool, a number, a string, a flag.
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Flagged { .. }
        )
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Number(Number::from_i64(value))
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Number(Number::from_f64(value))
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Value::Array(value)
    }
}

impl From<Object> for Value {
    fn from(value: Object) -> Self {
        Value::Object(value)
    }
}

/// A key as it was written: its text, and whether it was quoted in the source.
///
/// The flag only exists so a quoted-but-quotable key (`"exposed_params.g_flWearAmount"`) keeps
/// its quotes on write; keys inserted through [`Object::insert`] are quoted only when their text
/// is not a bare identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub text: String,
    pub quoted: bool,
}

/// An insertion-ordered map with unique keys.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Object {
    pub(crate) entries: Vec<(Key, Value)>,
}

impl Object {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k.text == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries.iter_mut().find(|(k, _)| k.text == key).map(|(_, v)| v)
    }

    /// Insert `key`; an existing key is replaced **in place**, keeping its position.
    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        let quoted = !is_plain_key(&key);
        self.insert_full(Key { text: key, quoted }, value)
    }

    pub(crate) fn insert_full(&mut self, key: Key, value: Value) -> Option<Value> {
        if let Some((_, slot)) = self.entries.iter_mut().find(|(k, _)| k.text == key.text) {
            return Some(std::mem::replace(slot, value));
        }
        self.entries.push((key, value));
        None
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let at = self.entries.iter().position(|(k, _)| k.text == key)?;
        Some(self.entries.remove(at).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.text.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.text.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when `key` was quoted in the source (or inserted with a non-identifier text).
    pub fn is_quoted(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k.text == key && k.quoted)
    }
}

/// The `<!-- kv3 … -->` header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The full line, without its line ending, exactly as read.
    pub raw: String,
    /// The `encoding:` value (`"text"`).
    pub encoding: String,
    /// The `format:` value (`"modeldoc36"`, `"modeldoc41"`, `"generic"`, …).
    pub format: String,
}

/// `modeldoc36` is the only non-`generic` format id this repo writes; its GUID comes from the
/// shipped `glass_probe.vmdl` header (header lines are text, they may be copied).
const MODELDOC36_GUID: &str = "972dada4-b828-45a4-bb93-7795cf0585da";
/// The text encoding's GUID (upstream `Encoding.Text`); every header must carry it.
pub(crate) const ENCODING_TEXT_GUID: &str = "e21c7f3c-8a33-41c5-9977-a76d3a32aa0d";
/// The `generic` format's GUID (upstream `Format.Generic`); validated for `generic` headers.
pub(crate) const FORMAT_GENERIC_GUID: &str = "7412167c-06e9-4698-aff2-e63eb59037e7";

impl Header {
    /// A copy of this header with another `format:` id, keeping the encoding field and, when the
    /// target has no known GUID, the current format version GUID.
    ///
    /// `modeldoc36` is the only non-`generic` format this repo writes and the only one whose GUID
    /// is recorded here (from the shipped `glass_probe.vmdl` header). Any other `modeldoc*` target
    /// (`modeldoc41`, what the editor saves) reuses that same modeldoc GUID: a modeldoc GUID is
    /// closer to the truth than the `generic` one, and the real `modeldoc41` GUID is unknown.
    pub fn with_format(&self, fmt: &str) -> Header {
        if fmt == self.format {
            return self.clone();
        }
        let guid = if fmt.starts_with("modeldoc") {
            MODELDOC36_GUID.to_string()
        } else {
            self.format_guid().unwrap_or(MODELDOC36_GUID).to_string()
        };
        let old = format!("format:{}:version{{", self.format);
        let raw = match self.raw.find(&old).and_then(|at| {
            let close = self.raw[at..].find('}')? + at;
            Some(format!(
                "{}format:{fmt}:version{{{guid}}}{}",
                &self.raw[..at],
                &self.raw[close + 1..]
            ))
        }) {
            Some(raw) => raw,
            // No `format:…:version{…}` field to rewrite: rebuild the line from the fields we have.
            None => format!(
                "<!-- kv3 encoding:{enc}:version{{{ENCODING_TEXT_GUID}}} format:{fmt}:version{{{guid}}} -->",
                enc = self.encoding
            ),
        };
        Header { raw, encoding: self.encoding.clone(), format: fmt.to_string() }
    }

    /// The `version{…}` GUID that follows the `format:` field.
    pub fn format_guid(&self) -> Option<&str> {
        let at = self.raw.find(&format!("format:{}", self.format))?;
        let rest = &self.raw[at..];
        let open = rest.find('{')?;
        let close = rest[open..].find('}')? + open;
        Some(&rest[open + 1..close])
    }
}

/// A parsed KV3 file: the header line and the single top-level object.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub header: Header,
    pub root: Object,
}

impl Document {
    pub fn new(header: Header) -> Self {
        Self { header, root: Object::new() }
    }
}

/// A syntax error, with a 1-based line and column (columns count characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}: {}", self.line, self.col, self.msg)
    }
}

impl std::error::Error for ParseError {}

/// The keys the writer prints bare: `[A-Za-z_][A-Za-z0-9_.]*`, the 686975d predicate. The
/// reader accepts more bare keys ([`is_identifier`]: a leading `.`), but a tree never *prints*
/// one bare, so the output stays what 686975d wrote and what its reader can read back.
pub(crate) fn is_plain_key(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Upstream `KV3TokenReader.IsIdentifier`: ASCII letters/digits/`_`/`.`, not empty, and never
/// starting with a digit (a leading `.` is fine — `DeserializesUnusualKeys` keys on `.y`).
pub(crate) fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_digit() {
        return false;
    }
    let allowed =
        |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    allowed(first) && chars.all(allowed)
}

/// Upstream `KV3TextReader.ParseValue`, as a lexeme check (the literals `null`/`true`/`false`
/// and the flag names live with the parser): a number is
///
/// * an i64 with an optional sign (`-1337`, `-0`, `-9223372036854775808`),
/// * a u64 with an optional leading `+` (`128`, `+1337`, `007`, `18446744073709551615`),
/// * a token of only `+-.0123456789Ee` that parses as an f64 (`.5`, `5.`, `1e5`,
///   `1.23456E+2`; overflow reads as infinity, like upstream's `double.TryParse`),
/// * or the float literals `inf` / `+inf` / `-inf` / `nan`, case-insensitive,
/// * or a bare `+` (upstream reads it as the integer 0).
pub(crate) fn is_number_lexeme(text: &str) -> bool {
    if text == "+" {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), "nan" | "inf" | "+inf" | "-inf") {
        return true;
    }
    if text.parse::<i64>().is_ok() {
        return true;
    }
    if text.parse::<u64>().is_ok() {
        return true;
    }
    if !text.bytes().all(|b| matches!(b, b'+' | b'-' | b'.' | b'0'..=b'9' | b'E' | b'e')) {
        return false;
    }
    text.parse::<f64>().is_ok()
}
