//! `serde_json` interop.
//!
//! The mapping is 1:1 for everything KV3 has a JSON spelling for, plus two tagged forms:
//!
//! | KV3 | JSON |
//! |---|---|
//! | `null`, `true`, `false`, number, `"string"` | the JSON value (a number keeps its lexeme; a lexeme JSON cannot hold stays a string) |
//! | `resource_name:"x"` | `{"$flag":"resource_name","value":"x"}` |
//! | `#[ 41 42 ]` | `{"$blob":"4142"}` (uppercase hex) |
//! | array | array |
//! | object | object, **keys in `serde_json`'s order (alphabetical)** — object key order survives parse/write, but not a `to_json`/`from_json` round trip |
//!
//! [`from_json`] is the inverse: an object with exactly the keys `$flag`/`value` or `$blob`
//! becomes a flag/blob (an object that merely *contains* `$flag` stays an object). The `$flag`
//! name is taken as-is: no flag name is validated here, and [`crate::parse`] accepts any
//! identifier as a flag too (upstream's seven canonical names are not enforced).

use crate::{Number, Object, Value};
use serde_json::{Map, Value as Json};
use std::fmt;

/// A JSON value that has no KV3 counterpart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvError {
    pub msg: String,
}

impl fmt::Display for ConvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ConvError {}

fn err(msg: impl Into<String>) -> ConvError {
    ConvError { msg: msg.into() }
}

pub const FLAG_KEY: &str = "$flag";
pub const BLOB_KEY: &str = "$blob";
pub const VALUE_KEY: &str = "value";

/// KV3 value -> JSON value.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                Json::from(i)
            } else if let Ok(u) = number.text.parse::<u64>() {
                Json::from(u)
            } else if let Some(f) = number.as_f64() {
                match serde_json::Number::from_f64(f) {
                    Some(n) => Json::Number(n),
                    None => Json::String(number.text.clone()),
                }
            } else {
                Json::String(number.text.clone())
            }
        }
        Value::String(text) => Json::String(text.clone()),
        Value::Flagged { flag, value } => {
            let mut map = Map::new();
            map.insert(FLAG_KEY.to_string(), Json::String(flag.clone()));
            map.insert(VALUE_KEY.to_string(), to_json(value));
            Json::Object(map)
        }
        Value::Blob(bytes) => {
            let mut map = Map::new();
            map.insert(BLOB_KEY.to_string(), Json::String(hex_upper(bytes)));
            Json::Object(map)
        }
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Object(object) => {
            let mut map = Map::new();
            for (key, value) in object.iter() {
                map.insert(key.to_string(), to_json(value));
            }
            Json::Object(map)
        }
    }
}

/// JSON value -> KV3 value.
pub fn from_json(value: &Json) -> Result<Value, ConvError> {
    Ok(match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(number) => {
            let text = number.to_string();
            match Number::parse(&text) {
                Some(parsed) => Value::Number(parsed),
                None => match number.as_f64() {
                    Some(f) => Value::Number(Number::from_f64(f)),
                    None => return Err(err(format!("number {text} has no kv3 spelling"))),
                },
            }
        }
        Json::String(text) => Value::String(text.clone()),
        Json::Array(items) => {
            Value::Array(items.iter().map(from_json).collect::<Result<Vec<_>, _>>()?)
        }
        Json::Object(map) => {
            if map.len() == 2 {
                if let (Some(Json::String(flag)), Some(value)) =
                    (map.get(FLAG_KEY), map.get(VALUE_KEY))
                {
                    return Ok(Value::Flagged {
                        flag: flag.clone(),
                        value: Box::new(from_json(value)?),
                    });
                }
            }
            if map.len() == 1 {
                if let Some(Json::String(hex)) = map.get(BLOB_KEY) {
                    return Ok(Value::Blob(hex_bytes(hex)?));
                }
            }
            let mut object = Object::new();
            for (key, value) in map {
                object.insert(key.clone(), from_json(value)?);
            }
            Value::Object(object)
        }
    })
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

fn hex_bytes(text: &str) -> Result<Vec<u8>, ConvError> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(err(format!("blob hex string has an odd length: {text:?}")));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    let mut nibbles = cleaned.chars().map(|c| {
        c.to_digit(16)
            .map(|d| d as u8)
            .ok_or_else(|| err(format!("`{c}` is not a hex digit in blob {text:?}")))
    });
    while let Some(high) = nibbles.next() {
        let high = high?;
        let low = nibbles.next().ok_or_else(|| err("truncated blob hex string"))??;
        out.push((high << 4) | low);
    }
    Ok(out)
}
