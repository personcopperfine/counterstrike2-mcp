//! `cs2-kv1` — Valve KeyValues1: the text format (`gameinfo.gi`, `*.vdf`, `*.res`, `*.txt`
//! scripts) and the binary format (Steam `appinfo` / `shortcuts.vdf`, `KeyValues::WriteAsBinary`).
//!
//! A Rust port of [ValveKeyValue](https://github.com/ValveResourceFormat/ValveKeyValue)
//! (MIT, © 2016 ValveKeyValue Contributors) — the KV1 half only; KV3 text is `cs2-kv3`.
//! The port follows the upstream readers/writers and their test suite; every deliberate
//! difference is listed in the module docs of [`text`] and [`binary`].
//!
//! The data model is deliberately small and **lossless for order and duplicates**: an
//! [`Object`] is a `Vec` of [`KeyValue`] pairs, keys may repeat (KV1 files rely on it, e.g.
//! `SearchPaths` in `gameinfo.gi`), and the order read is the order written.

#![forbid(unsafe_code)]

pub mod binary;
pub mod text;

/// One `key value` pair. In text, a value is a string or a `{ … }` block.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyValue {
    pub key: String,
    pub value: Value,
    /// The `[$COND]` that followed the pair in text, verbatim without the brackets
    /// (`$WIN32`, `!$X360 && $PC`). Only set when the text was read with
    /// [`text::ReadOptions::conditions`] = `None` (keep every pair); with a condition list the
    /// reader evaluates and drops pairs instead, and this is `None`.
    pub condition: Option<String>,
}

/// A KV1 value. Text KV1 only produces [`Value::String`] and [`Value::Object`]; the other
/// variants are the binary node types (`KV1BinaryNodeType` upstream).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Object(Object),
    String(String),
    Int32(i32),
    Float32(f32),
    /// Binary type 4: a 32-bit pointer value.
    Pointer(i32),
    /// Binary type 5. Upstream reads it as UTF-16 (see [`binary`] for what is supported).
    WideString(String),
    /// Binary type 6: four bytes, in stream order.
    Color([u8; 4]),
    UInt64(u64),
    Int64(i64),
    /// Binary type 9.
    Binary(Vec<u8>),
}

/// An ordered list of pairs; duplicate keys allowed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Object {
    pub entries: Vec<KeyValue>,
}

/// A whole document: the root pair (`"GameInfo" { … }`). Upstream `KVDocument`.
pub type Document = KeyValue;

impl Object {
    pub fn new() -> Self {
        Self::default()
    }
    /// First value for `key`, **case-insensitive** (KV1 keys are; upstream indexer semantics).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|kv| kv.key.eq_ignore_ascii_case(key)).map(|kv| &kv.value)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries.iter_mut().find(|kv| kv.key.eq_ignore_ascii_case(key)).map(|kv| &mut kv.value)
    }
    /// Every value for `key` in order (case-insensitive).
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        self.entries.iter().filter(move |kv| kv.key.eq_ignore_ascii_case(key)).map(|kv| &kv.value)
    }
    /// Append a pair (never replaces).
    pub fn push(&mut self, key: impl Into<String>, value: Value) {
        self.entries.push(KeyValue { key: key.into(), value, condition: None });
    }
}

impl Value {
    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::WideString(s) => Some(s),
            _ => None,
        }
    }
}

/// Every error either half can return. Positions are 1-based line/column in text, byte
/// offsets in binary.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("line {line}, column {column}: {message}")]
    Syntax { line: usize, column: usize, message: String },
    #[error("byte {offset}: {message}")]
    Binary { offset: usize, message: String },
    #[error("include {path:?}: {message}")]
    Include { path: String, message: String },
}
