//! keyvalues2 / keyvalues2_noids, versions 1–4. A port of upstream `Codecs/KeyValues2.cs`
//! ([Datamodel.NET](https://github.com/ValveResourceFormat/Datamodel.NET), commit 7d4d6e1).
//!
//! The layout comes byte for byte from the C# writer: tab indentation, LF newlines,
//! `"name" "type" "value"` triples, one array item per line, a comma after every array item
//! but the last, a blank line after every inline element attribute and after every top-level
//! element block, and a **trailing space after the type token of an array header**
//! (`"refs" "string_array" `). Elements referenced more than once (upstream counts references
//! in a depth-first walk from the root, decided purely by reference frequency) come out as
//! separate blocks after the root and are referred to by GUID; everything else is written
//! inline where it is referenced.
//!
//! Floats print the way .NET's `((double)value).ToString("0.##########")` does: the double is
//! rounded to **15 significant digits** first (half to even), that decimal is rounded half up at
//! the **tenth decimal**, and trailing zeros are dropped. `40` stays `40`, `0.1f` prints
//! `0.1000000015`, `-270.11304f` prints `-270.1130371094`, `2^-11` prints `0.0004882813`, and a
//! value past 15 digits is zero padded (`3.4e38f` prints `339999995214436000000000000000000000000`).
//! The rule is checked against .NET 10 itself by `tests/data/float*_format_table.txt`, generated
//! by `tests/dotnet/FloatTable`. Both layouts use it: Valve's real writer is not available to
//! check, the corpus holds no value where the rules would differ (no tie, nothing past 15
//! digits), and the .NET rule is the only one that has been verified. `uint64` gets a
//! lowercase-hex `0x` prefix; `binary` prints upper-case hex run together.
//!
//! # The two layouts
//!
//! Two places in this format are left to the writer, and Valve's tools and upstream's writer
//! chose differently. [`Layout::Valve`] is the default ([`encode`], and so [`crate::save`]),
//! because it is what makes a file Valve's tools wrote come back byte for byte:
//!
//! * the blocks after the root come in [`Datamodel::elements`] order — the order the file
//!   defines its elements in, which is the order a reader builds the arena in — where upstream
//!   uses the order its reference walk first met each element;
//! * a `binary` value longer than [`BINARY_WRAP_COLUMNS`] hex digits is wrapped: the name and
//!   type close their line, the value opens on a line of its own, and the hex runs 40 bytes to
//!   a line one indent deeper. The line breaks and tabs sit *inside* the quoted token, which
//!   the reference reader strips off a binary value again. Upstream writes every `binary`
//!   value, however long, on the attribute line. **The 40-byte width and the "wrap only past
//!   one line" threshold are inferred from one sample**, the 18598-byte thumbnail in
//!   `reflectiontest.vmap.txt`; no shorter wrapped value and no unwrapped long one is in the
//!   corpus to confirm where Valve's writer starts to wrap.
//!
//! The two real `.vmap.txt` sample files in `Datamodel.NET/Tests/Resources` exercise both
//! (they were written by Valve's text exporter, not by upstream) and round-trip byte for byte.
//! [`Layout::Upstream`] is the C# writer's layout, which the upstream tests pin; it is a model
//! round trip on the same files, not a byte round trip.
//!
//! # Differences from upstream
//!
//! * **The `$prefix_element$` block stays an element *and* the prefix attributes.** Upstream
//!   folds the block into `Datamodel.PrefixAttributes` and throws away its id line
//!   (`PrefixElementId`), so nothing in its model says the block existed apart from the
//!   attributes. Here the block is an element in [`Datamodel::elements`] at its file position,
//!   whose class is `$prefix_element$`, whose GUID is the block's, and whose attributes mirror
//!   [`Datamodel::prefix_attributes`]; the element is never the root and nothing resolves to it
//!   by GUID, because upstream's element list does not hold it either. A second
//!   `$prefix_element$` block never makes an element: it merges its attributes into the prefix
//!   attributes and sets the GUID (the last block's wins), like upstream's `PrefixElementId`;
//!   an inline element of that class is a null to its parent. [`encode`] writes the block once,
//!   from [`Datamodel::prefix_attributes`] (so an edit to them shows), with the GUID of the
//!   model's [`Datamodel::prefix_element`] — which also finds the element a binary 9 file
//!   leaves there, so a model from either codec writes the right block and no second copy of
//!   that element. A model with `prefix_attributes` and no such element still gets the block,
//!   under the all-zero GUID — the frozen [`crate::Datamodel`] has no `PrefixElementId` field
//!   (upstream writes a fresh random GUID there). Upstream writes no block when there are no
//!   prefix attributes; [`Layout::Upstream`] does the same, [`Layout::Valve`] keeps a block that
//!   holds only its id line, which a file can contain. The block's GUID is read in
//!   `keyvalues2_noids` too (upstream writes that id line there as well), so such a file reads
//!   back byte for byte.
//! * **keyvalues2_noids decoding is implemented.** Upstream throws `NotImplementedException`
//!   for it. Without ids a file still parses: every element is built eagerly at the `{` of
//!   its block, takes no id line and ends up with a `Guid::default()` GUID, so decoding and
//!   encoding again reproduce the same text (the noids writer never prints ids, and with
//!   `SupportsReferenceIds` off it writes every reference inline — no GUID is ever printed).
//!   One id line inside a noids element is tolerated and skipped (a second is an error, as in
//!   keyvalues2). A model the noids writer cannot inline — one with a reference cycle — is an
//!   `Err` (the depth cap), where upstream's writer recurses until the stack dies; a reference
//!   met twice is simply written inline twice.
//! * **A second `id` line on an element is an error** ("Expected Element opener", as upstream:
//!   it looks for the id only while the element does not exist). A `$prefix_element$` block takes
//!   any number of them.
//! * **Encoding refuses what could not read back**: two reachable elements sharing one GUID
//!   (keyvalues2; noids writes no ids), versions outside 1-4, encodings other than
//!   keyvalues2/keyvalues2_noids. The binary codec does not check duplicate GUIDs on encode.
//! * **An element array redefined under the same name** keeps only the references of its
//!   last definition; upstream patches the replaced list, which nothing holds.
//! * **Inline nesting is capped** ([`MAX_KV2_DEPTH`]) on both sides: a file that nests more
//!   than that many elements, and a model that nests more, return `Err`. Upstream recurses per
//!   inline element and dies with a stack overflow on a crafted 10 000-deep decode. The
//!   reference-counting walk, whose upstream recursion cannot bound either, is iterative.
//! * **References the file never defines stay null**, as upstream: `dataModel.AllElements[id]`
//!   is null and the attribute becomes a null reference. Only an unparsable GUID text errors,
//!   because that is `new Guid(string)` throwing. A decoded model never marks anything
//!   `stub`: the text format cannot express stubness, and the upstream writer itself turns a
//!   once-referenced stub into an ordinary empty inline element.
//! * **Attributes met before their element exists** follow upstream exactly, including the
//!   corners: they land in `prefix_attributes` (not on the element), an element reference
//!   there is an `Err` when it names a GUID and dropped when the GUID text is empty, and an
//!   inline element that *is* an element there is refused the way upstream's `AttributeList`
//!   refuses Element values ("Elements are not supported as prefix attributes").
//! * **A broken model is an `Err`.** A hand-built [`crate::Datamodel`] whose root or whose
//!   element reference points outside its own arena is refused before anything is written,
//!   where the C# writer would walk into an `IndexOutOfRangeException` or a null dereference.
//! * Defined-for-robustness details the C# leaves undefined: the body is decoded lossily
//!   (invalid UTF-8 becomes U+FFFD, as .NET's default `StreamReader` does); non-finite floats
//!   encode as `NaN` / `Infinity` / `-Infinity` (.NET's invariant-culture symbols) and decode
//!   back; an out-of-range time saturates, as .NET 10's `(long)` cast of a double does.
//! * **Number text follows .NET 10's grammar** (checked against it): `Infinity` and `NaN` in
//!   any case are floats, `inf` is not; `+1`, `.5`, `5.` and `1E+2` are; `byte.Parse` takes
//!   `-0`; a `uint64` value is `Remove(0, 2)` (two UTF-16 units, whatever they are, so `€1` and
//!   `é1` are errors and `0x0x7` is too) followed by `HexNumber` (white space around bare hex
//!   digits, no sign, no second `0x`). **Not implemented:** .NET also accepts digit group
//!   separators in floats (`1,000` is 1000, `1,,2` is 12); nothing writes them, so this reader
//!   refuses them.
//! * GUID text goes through [`Guid`]'s `FromStr`: `D`, `N`, brace and parenthesis wrapped forms,
//!   any hex case, surrounding white space, as .NET's `Guid(string)` takes them (not the exotic
//!   `X` form no DMX file uses). The first three text groups are numbers and reach the stream
//!   little-endian, exactly like `Guid.ToByteArray()`.

use std::collections::HashMap;

use super::format_header;
use super::Header;
use crate::{Attribute, Datamodel, Element, ElementId, Error, Guid, Time};

/// Maximum inline-element depth on either side; going past it is an explicit `Err`, never a
/// stack overflow. Real files nest far less.
pub const MAX_KV2_DEPTH: usize = 256;

/// The class name of the block that carries the prefix attributes from version 4 on.
pub const PREFIX_ELEMENT_CLASS: &str = "$prefix_element$";

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_pair(bytes: &[u8]) -> Option<u8> {
    Some(hex_digit(bytes[0])? * 16 + hex_digit(bytes[1])?)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Upstream `KV2Writer.Sanitise`: backslash first, then the quote and the three control
/// characters; applied to every quoted token the writer emits.
fn sanitise(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Upstream `KV2Writer.Token`: a quoted, sanitised string.
fn token(value: &str) -> String {
    format!("\"{}\"", sanitise(value))
}

// ---------------------------------------------------------------------------
// Type names
// ---------------------------------------------------------------------------

/// The bare type word an attribute's type token names — what upstream resolves against with
/// `TypeNames.FirstOrDefault(...attr_type_s.Split('_')[0])`. `None` makes the token count as
/// an element class name instead, so `foo_array` decodes as an inline element, exactly as
/// upstream lets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseType {
    Element,
    Int,
    Float,
    Bool,
    String,
    Binary,
    Time,
    Color,
    Vector2,
    Vector3,
    Vector4,
    Quaternion,
    Matrix,
    QAngle,
    UInt8,
    UInt64,
    None,
}

impl BaseType {
    fn of(type_token: &str) -> BaseType {
        let base = type_token.split('_').next().unwrap_or("");
        match base {
            "element" => BaseType::Element,
            "int" => BaseType::Int,
            "float" => BaseType::Float,
            "bool" => BaseType::Bool,
            "string" => BaseType::String,
            "binary" => BaseType::Binary,
            "time" => BaseType::Time,
            "color" => BaseType::Color,
            "vector2" => BaseType::Vector2,
            "vector3" => BaseType::Vector3,
            "vector4" => BaseType::Vector4,
            "quaternion" => BaseType::Quaternion,
            "matrix" => BaseType::Matrix,
            "qangle" => BaseType::QAngle,
            "uint8" => BaseType::UInt8,
            "uint64" => BaseType::UInt64,
            _ => BaseType::None,
        }
    }
}

/// The scalar type name of an attribute value, without any `_array` suffix — upstream
/// `TypeNames`. `None` for `element` values (written elsewhere) and for arrays.
fn scalar_type_name(attr: &Attribute) -> Option<&'static str> {
    Some(match attr {
        Attribute::Int(_) => "int",
        Attribute::Float(_) => "float",
        Attribute::Bool(_) => "bool",
        Attribute::String(_) => "string",
        Attribute::Binary(_) => "binary",
        Attribute::Time(_) => "time",
        Attribute::Color(_) => "color",
        Attribute::Vector2(_) => "vector2",
        Attribute::Vector3(_) => "vector3",
        Attribute::Vector4(_) => "vector4",
        Attribute::QAngle(_) => "qangle",
        Attribute::Quaternion(_) => "quaternion",
        Attribute::Matrix(_) => "matrix",
        Attribute::UInt64(_) => "uint64",
        Attribute::Byte(_) => "uint8",
        _ => return None,
    })
}

/// The inner type name of an array value, what `Datamodel.GetArrayInnerType` returns. A
/// `Binary(Vec<u8>)` is always written as `binary`, never `uint8_array` — upstream has one
/// CLR type for both and special-cases the writer; the port's separate `Attribute::Binary` /
/// `Attribute::ByteArray` variants make the rule fall out on its own.
fn array_inner_type(attr: &Attribute) -> Option<&'static str> {
    Some(match attr {
        Attribute::ElementArray(_) => "element",
        Attribute::IntArray(_) => "int",
        Attribute::FloatArray(_) => "float",
        Attribute::BoolArray(_) => "bool",
        Attribute::StringArray(_) => "string",
        Attribute::BinaryArray(_) => "binary",
        Attribute::TimeArray(_) => "time",
        Attribute::ColorArray(_) => "color",
        Attribute::Vector2Array(_) => "vector2",
        Attribute::Vector3Array(_) => "vector3",
        Attribute::Vector4Array(_) => "vector4",
        Attribute::QAngleArray(_) => "qangle",
        Attribute::QuaternionArray(_) => "quaternion",
        Attribute::MatrixArray(_) => "matrix",
        Attribute::UInt64Array(_) => "uint64",
        Attribute::ByteArray(_) => "uint8",
        _ => return None,
    })
}

/// Upstream `ValidAttributes`: `uint8`, `uint64` and `qangle` only exist from version 4 on.
fn valid_in_version(type_name: &str, encoding_version: i32) -> bool {
    !matches!(type_name, "uint8" | "uint64" | "qangle") || encoding_version >= 4
}

/// The class names `WriteElementBody` refuses — anything the type table could legally
/// intercept as a type token; `elementid` is deliberately *not* reserved, like upstream.
const RESERVED_TYPE_NAMES: [&str; 16] = [
    "element", "int", "float", "bool", "string", "binary", "time", "color", "vector2",
    "vector3", "vector4", "quaternion", "matrix", "uint8", "uint64", "qangle",
];

/// Every attribute variant's name, for error text.
fn attr_variant_name(attr: &Attribute) -> &'static str {
    match attr {
        Attribute::Element(_) => "Element",
        Attribute::Int(_) => "Int",
        Attribute::Float(_) => "Float",
        Attribute::Bool(_) => "Bool",
        Attribute::String(_) => "String",
        Attribute::Binary(_) => "Binary",
        Attribute::Time(_) => "Time",
        Attribute::Color(_) => "Color",
        Attribute::Vector2(_) => "Vector2",
        Attribute::Vector3(_) => "Vector3",
        Attribute::Vector4(_) => "Vector4",
        Attribute::QAngle(_) => "QAngle",
        Attribute::Quaternion(_) => "Quaternion",
        Attribute::Matrix(_) => "Matrix",
        Attribute::UInt64(_) => "UInt64",
        Attribute::Byte(_) => "Byte",
        Attribute::ElementArray(_) => "ElementArray",
        Attribute::IntArray(_) => "IntArray",
        Attribute::FloatArray(_) => "FloatArray",
        Attribute::BoolArray(_) => "BoolArray",
        Attribute::StringArray(_) => "StringArray",
        Attribute::BinaryArray(_) => "BinaryArray",
        Attribute::TimeArray(_) => "TimeArray",
        Attribute::ColorArray(_) => "ColorArray",
        Attribute::Vector2Array(_) => "Vector2Array",
        Attribute::Vector3Array(_) => "Vector3Array",
        Attribute::Vector4Array(_) => "Vector4Array",
        Attribute::QAngleArray(_) => "QAngleArray",
        Attribute::QuaternionArray(_) => "QuaternionArray",
        Attribute::MatrixArray(_) => "MatrixArray",
        Attribute::UInt64Array(_) => "UInt64Array",
        Attribute::ByteArray(_) => "ByteArray",
    }
}

// ---------------------------------------------------------------------------
// Float formatting
// ---------------------------------------------------------------------------

/// Upstream `FormatFloat`: `((double)value).ToString("0.##########", InvariantCulture)`.
/// Fixed notation with up to ten decimals, trailing zeros dropped, always with a digit before
/// the point (`40`, `1`, `-0`, small values printing bare `0`). Non-finite values print in the
/// invariant-culture words .NET uses inside any custom numeric format.
///
/// .NET does not cut the exact expansion of the double at ten decimals. It first rounds the
/// double to **15 significant digits** (half to even on the exact value; the digits past the 15th
/// read as zeros afterwards, so `2^60` prints `1152921504606850000`, not
/// `1152921504606846976`), and then rounds that decimal **half up** at the tenth decimal. Both
/// steps are checked against .NET 10 by `tests/data/float*_format_table.txt`.
fn format_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if !value.is_finite() {
        return if value.is_sign_negative() { "-Infinity".to_string() } else { "Infinity".to_string() };
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0".to_string() } else { "0".to_string() };
    }
    let (negative, significand, exponent) = decompose(value).expect("a finite non-zero value decomposes");
    let (mut digits, mut scale) = exact_decimal(significand, exponent);

    // step 1: 15 significant digits, half to even on the exact expansion
    if digits.len() > 15 {
        let cut = digits.len() - 15;
        let rest = &digits[15..];
        let first = rest[0];
        let beyond_zero = rest[1..].iter().all(|digit| *digit == b'0');
        let up = first > b'5' || (first == b'5' && (!beyond_zero || (digits[14] - b'0') % 2 == 1));
        digits.truncate(15);
        if up {
            increment(&mut digits);
        }
        scale -= cut as i64;
    }
    // what the 15 digits no longer hold reads as zeros
    if scale < 0 {
        digits.extend(std::iter::repeat_n(b'0', (-scale) as usize));
        scale = 0;
    }

    // step 2: half up at the tenth decimal
    let scale = scale as usize;
    let total: Vec<u8> = if scale > 10 {
        let drop = scale - 10;
        if drop > digits.len() {
            vec![b'0']
        } else {
            let round_up = digits[digits.len() - drop] >= b'5';
            digits.truncate(digits.len() - drop);
            if digits.is_empty() {
                digits.push(b'0');
            }
            if round_up {
                increment(&mut digits);
            }
            digits
        }
    } else {
        digits.extend(std::iter::repeat_n(b'0', 10 - scale));
        digits
    };
    fixed_10(negative, std::str::from_utf8(&total).expect("digits stay ascii"))
}

/// Add one to a string of ASCII decimal digits, growing it on carry out.
fn increment(digits: &mut Vec<u8>) {
    for digit in digits.iter_mut().rev() {
        if *digit == b'9' {
            *digit = b'0';
        } else {
            *digit += 1;
            return;
        }
    }
    digits.insert(0, b'1');
}

/// The exact decimal expansion of `significand · 2^exponent` as `(digits, scale)`: the value is
/// the digit string times `10^-scale`. A negative exponent turns into `significand · 5^k / 10^k`.
fn exact_decimal(significand: u64, exponent: i32) -> (Vec<u8>, i64) {
    // little-endian base 10^9 limbs
    const BASE: u64 = 1_000_000_000;
    fn mul_small(limbs: &mut Vec<u32>, factor: u64) {
        let mut carry = 0u64;
        for limb in limbs.iter_mut() {
            let product = u64::from(*limb) * factor + carry;
            *limb = (product % BASE) as u32;
            carry = product / BASE;
        }
        while carry > 0 {
            limbs.push((carry % BASE) as u32);
            carry /= BASE;
        }
    }
    let mut limbs: Vec<u32> = vec![(significand % BASE) as u32, ((significand / BASE) % BASE) as u32, (significand / BASE / BASE) as u32];
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let scale = if exponent >= 0 {
        let mut left = exponent as u32;
        while left > 0 {
            let step = left.min(29); // 2^29 · 10^9 stays inside u64
            mul_small(&mut limbs, 1u64 << step);
            left -= step;
        }
        0
    } else {
        let mut left = -(exponent as i64);
        let scale = left;
        while left > 0 {
            let step = left.min(13); // 5^13 < 2^32
            mul_small(&mut limbs, 5u64.pow(step as u32));
            left -= step;
        }
        scale
    };
    let mut text = String::new();
    for (index, limb) in limbs.iter().rev().enumerate() {
        if index == 0 {
            text.push_str(&limb.to_string());
        } else {
            text.push_str(&format!("{limb:09}"));
        }
    }
    (text.into_bytes(), scale)
}

/// Fixed-point rendering with the point ten digits in from the right, as .NET's
/// "0.##########" crops it: trailing fraction zeros trimmed, zero keeps its sign.
fn fixed_10(negative: bool, total: &str) -> String {
    let signed = |text: String| if negative { format!("-{text}") } else { text };
    if total.len() <= 10 {
        let frac = format!("{:0>10}", total).trim_end_matches('0').to_string();
        if total.bytes().all(|digit| digit == b'0') {
            return signed(if frac.is_empty() { "0".to_string() } else { format!("0.{frac}") });
        }
        return signed(format!("0.{frac}"));
    }
    let frac = total[total.len() - 10..].trim_end_matches('0');
    if frac.is_empty() {
        return signed(total[..total.len() - 10].to_string());
    }
    signed(format!("{}.{frac}", &total[..total.len() - 10]))
}

/// `(negative, value exactly = significand · 2^exponent)` for finite non-zero values.
fn decompose(value: f64) -> Option<(bool, u64, i32)> {
    if !value.is_finite() || value == 0.0 {
        return None;
    }
    let bits = value.to_bits();
    let negative = bits >> 63 == 1;
    let biased = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & ((1u64 << 52) - 1);
    Some(if biased == 0 {
        (negative, mantissa, -1074)
    } else {
        (negative, mantissa | (1 << 52), (biased - 1023 - 52) as i32)
    })
}

/// Upstream `FormatValue` for a scalar value (elements format elsewhere; arrays through
/// [`format_array_item`]).
fn format_value(attr: &Attribute) -> Result<String, Error> {
    Ok(match attr {
        Attribute::String(value) => value.clone(), // quoted and sanitised by the caller
        Attribute::Bool(value) => (if *value { "1" } else { "0" }).to_string(),
        Attribute::Int(value) => value.to_string(),
        Attribute::Float(value) => format_float(*value as f64),
        Attribute::Byte(value) => value.to_string(),
        Attribute::UInt64(value) => format!("0x{value:x}"),
        Attribute::Binary(bytes) => hex_string(bytes),
        Attribute::Time(time) => format_float(time.0 as f32 as f64),
        Attribute::Color(color) => format!("{} {} {} {}", color[0], color[1], color[2], color[3]),
        Attribute::Vector2(v) => join_floats(v),
        Attribute::Vector3(v) | Attribute::QAngle(v) => join_floats(v),
        Attribute::Vector4(v) | Attribute::Quaternion(v) => join_floats(v),
        Attribute::Matrix(m) => m
            .iter()
            .flatten()
            .map(|component| format_float(*component as f64))
            .collect::<Vec<_>>()
            .join(" "),
        _ => {
            return Err(Error::Encode(format!(
                "Cannot serialize a value of type {} to KeyValues2",
                attr_variant_name(attr)
            )));
        }
    })
}

fn join_floats(values: &[f32]) -> String {
    values.iter().map(|c| format_float(*c as f64)).collect::<Vec<_>>().join(" ")
}

/// Upper-case hex, run together: `Convert.ToHexString`, and what Hammer's wrapped binary lines
/// hold too.
fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// `FormatValue` for one array item at `index`; every array's inner value is one of the very
/// same scalars.
fn format_array_item(array: &Attribute, index: usize) -> Result<String, Error> {
    let scalar = match array {
        Attribute::IntArray(items) => Attribute::Int(items[index]),
        Attribute::FloatArray(items) => Attribute::Float(items[index]),
        Attribute::BoolArray(items) => Attribute::Bool(items[index]),
        Attribute::StringArray(items) => Attribute::String(items[index].clone()),
        Attribute::BinaryArray(items) => Attribute::Binary(items[index].clone()),
        Attribute::TimeArray(items) => Attribute::Time(items[index]),
        Attribute::ColorArray(items) => Attribute::Color(items[index]),
        Attribute::Vector2Array(items) => Attribute::Vector2(items[index]),
        Attribute::Vector3Array(items) => Attribute::Vector3(items[index]),
        Attribute::Vector4Array(items) => Attribute::Vector4(items[index]),
        Attribute::QAngleArray(items) => Attribute::QAngle(items[index]),
        Attribute::QuaternionArray(items) => Attribute::Quaternion(items[index]),
        Attribute::MatrixArray(items) => Attribute::Matrix(items[index]),
        Attribute::UInt64Array(items) => Attribute::UInt64(items[index]),
        Attribute::ByteArray(items) => Attribute::Byte(items[index]),
        _ => {
            return Err(Error::Encode(format!(
                "Cannot serialize a value of type {} to KeyValues2",
                attr_variant_name(array)
            )));
        }
    };
    format_value(&scalar)
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// An element attribute whose slot already holds a null element and whose GUID target fills
/// in once the whole file has been parsed (upstream `IntermediateData.PropertiesToAdd`).
struct PendingRef {
    owner: ElementId,
    attr: String,
    guid: Guid,
}

/// Where one element-array slot reference resolves (upstream `ListRefs`): a direct attribute
/// of an element, or a prefix attribute.
enum ArrayOwner {
    Element(ElementId, String),
    Prefix(String),
}

struct PendingListRef {
    owner: ArrayOwner,
    /// Which parsed array this slot belongs to (see [`Parser::array_serials`]).
    serial: u64,
    index: usize,
    guid: Guid,
}

/// A value array under construction, one buffer per inner type (upstream builds one of the
/// fifteen typed `XxxArray` containers).
enum ValueBuf {
    Int(Vec<i32>),
    Float(Vec<f32>),
    Bool(Vec<bool>),
    String(Vec<String>),
    Binary(Vec<Vec<u8>>),
    Time(Vec<Time>),
    Color(Vec<[u8; 4]>),
    Vector2(Vec<[f32; 2]>),
    Vector3(Vec<[f32; 3]>),
    Vector4(Vec<[f32; 4]>),
    QAngle(Vec<[f32; 3]>),
    Quaternion(Vec<[f32; 4]>),
    Matrix(Vec<[[f32; 4]; 4]>),
    UInt8(Vec<u8>),
    UInt64(Vec<u64>),
}

impl ValueBuf {
    fn new(attr_type: BaseType) -> Result<ValueBuf, Error> {
        Ok(match attr_type {
            BaseType::Int => ValueBuf::Int(Vec::new()),
            BaseType::Float => ValueBuf::Float(Vec::new()),
            BaseType::Bool => ValueBuf::Bool(Vec::new()),
            BaseType::String => ValueBuf::String(Vec::new()),
            BaseType::Binary => ValueBuf::Binary(Vec::new()),
            BaseType::Time => ValueBuf::Time(Vec::new()),
            BaseType::Color => ValueBuf::Color(Vec::new()),
            BaseType::Vector2 => ValueBuf::Vector2(Vec::new()),
            BaseType::Vector3 => ValueBuf::Vector3(Vec::new()),
            BaseType::Vector4 => ValueBuf::Vector4(Vec::new()),
            BaseType::QAngle => ValueBuf::QAngle(Vec::new()),
            BaseType::Quaternion => ValueBuf::Quaternion(Vec::new()),
            BaseType::Matrix => ValueBuf::Matrix(Vec::new()),
            BaseType::UInt8 => ValueBuf::UInt8(Vec::new()),
            BaseType::UInt64 => ValueBuf::UInt64(Vec::new()),
            // the array branch only ever runs for a *known* scalar type
            BaseType::None | BaseType::Element => return Err(Error::Encode(
                "element attributes cannot form a value array".to_string(),
            )),
        })
    }

    /// Append one parsed scalar. Every buffer is built for and fed from exactly one base
    /// type, so only the call's own construction can break the pairing.
    fn push(&mut self, attr: Attribute) -> Result<(), Error> {
        match (&mut *self, attr) {
            (ValueBuf::Int(items), Attribute::Int(value)) => items.push(value),
            (ValueBuf::Float(items), Attribute::Float(value)) => items.push(value),
            (ValueBuf::Bool(items), Attribute::Bool(value)) => items.push(value),
            (ValueBuf::String(items), Attribute::String(value)) => items.push(value),
            (ValueBuf::Binary(items), Attribute::Binary(value)) => items.push(value),
            (ValueBuf::Time(items), Attribute::Time(value)) => items.push(value),
            (ValueBuf::Color(items), Attribute::Color(value)) => items.push(value),
            (ValueBuf::Vector2(items), Attribute::Vector2(value)) => items.push(value),
            (ValueBuf::Vector3(items), Attribute::Vector3(value)) => items.push(value),
            (ValueBuf::Vector4(items), Attribute::Vector4(value)) => items.push(value),
            (ValueBuf::QAngle(items), Attribute::QAngle(value)) => items.push(value),
            (ValueBuf::Quaternion(items), Attribute::Quaternion(value)) => items.push(value),
            (ValueBuf::Matrix(items), Attribute::Matrix(value)) => items.push(value),
            (ValueBuf::UInt8(items), Attribute::Byte(value)) => items.push(value),
            (ValueBuf::UInt64(items), Attribute::UInt64(value)) => items.push(value),
            (buffer, attr) => {
                return Err(Error::Decode {
                    at: "array item".to_string(),
                    message:	format!(
                        "a {} item cannot fill a {} array",
                        attr_variant_name(&attr),
                        buffer.kind().unwrap_or("(none)")
                    ),
                });
            }
        };
        Ok(())
    }

    fn kind(&self) -> Option<&'static str> {
        Some(match self {
            ValueBuf::Int(_) => "int",
            ValueBuf::Float(_) => "float",
            ValueBuf::Bool(_) => "bool",
            ValueBuf::String(_) => "string",
            ValueBuf::Binary(_) => "binary",
            ValueBuf::Time(_) => "time",
            ValueBuf::Color(_) => "color",
            ValueBuf::Vector2(_) => "vector2",
            ValueBuf::Vector3(_) => "vector3",
            ValueBuf::Vector4(_) => "vector4",
            ValueBuf::QAngle(_) => "qangle",
            ValueBuf::Quaternion(_) => "quaternion",
            ValueBuf::Matrix(_) => "matrix",
            ValueBuf::UInt8(_) => "uint8",
            ValueBuf::UInt64(_) => "uint64",
        })
    }

    fn into_attribute(self) -> Attribute {
        match self {
            ValueBuf::Int(items) => Attribute::IntArray(items),
            ValueBuf::Float(items) => Attribute::FloatArray(items),
            ValueBuf::Bool(items) => Attribute::BoolArray(items),
            ValueBuf::String(items) => Attribute::StringArray(items),
            ValueBuf::Binary(items) => Attribute::BinaryArray(items),
            ValueBuf::Time(items) => Attribute::TimeArray(items),
            ValueBuf::Color(items) => Attribute::ColorArray(items),
            ValueBuf::Vector2(items) => Attribute::Vector2Array(items),
            ValueBuf::Vector3(items) => Attribute::Vector3Array(items),
            ValueBuf::Vector4(items) => Attribute::Vector4Array(items),
            ValueBuf::QAngle(items) => Attribute::QAngleArray(items),
            ValueBuf::Quaternion(items) => Attribute::QuaternionArray(items),
            ValueBuf::Matrix(items) => Attribute::MatrixArray(items),
            ValueBuf::UInt8(items) => Attribute::ByteArray(items),
            ValueBuf::UInt64(items) => Attribute::UInt64Array(items),
        }
    }
}

/// The decoder's state; parse locals reflect the C# reader.
struct Parser {
    /// The body decoded the lossy way a default .NET `StreamReader` does, as characters.
    chars: Vec<char>,
    pos: usize,
    /// 1-based line counter, advanced by every `\r` and `\n` like the C# tokenizer's count.
    line: usize,
    /// keyvalues2_noids: build elements eagerly at their block, keep their default GUIDs out
    /// of the duplicate-id registry, skip id lines.
    noids: bool,
    dm: Datamodel,
    guid_seen: HashMap<Guid, ElementId>,
    /// The element built for the file's `$prefix_element$` block (the first one), if any: it
    /// stays in `elements` with the block's GUID, and mirrors `prefix_attributes` (see the
    /// module docs).
    prefix_element: Option<ElementId>,
    pending: Vec<PendingRef>,
    list_refs: Vec<PendingListRef>,
    /// The identity of the element array each `(owner, name)` slot holds right now, so a later
    /// attribute of the same name that replaces the array takes the pending slot references of
    /// the array it replaced with it: upstream patches the replaced list, which nothing holds any
    /// more. `None` as the owner is the prefix attributes.
    array_serials: HashMap<(Option<ElementId>, String), u64>,
    next_serial: u64,
    /// Where each `(owner, name)` attribute sits in its list (`None`: prefix attributes).
    attr_pos: HashMap<(Option<ElementId>, String), usize>,
}

/// Skip the header line the way upstream's `reader.ReadLine()` does — everything through the
/// next line ending (`\r`, `\n` or `\r\n`), and the whole rest when there is none. A body that
/// shares the header's line is lost, exactly as it is in C#.
fn skip_header_line(rest: &[u8]) -> &[u8] {
    match rest.iter().position(|&byte| byte == b'\n' || byte == b'\r') {
        Some(at) => {
            let mut start = at + 1;
            if rest[at] == b'\r' && rest.get(start) == Some(&b'\n') {
                start += 1;
            }
            &rest[start..]
        }
        None => &[],
    }
}

/// Decode a keyvalues2 / keyvalues2_noids body. `rest` is everything after the header's
/// closing `>`; the header line is skipped first, which is what upstream's rewind-then-
/// `ReadLine` achieves.
pub fn decode(header: &Header, rest: &[u8]) -> Result<Datamodel, Error> {
    let body = skip_header_line(rest);
    let mut parser = Parser {
        chars: String::from_utf8_lossy(body).into_owned().chars().collect(),
        pos: 0,
        line: 1,
        noids: header.encoding == "keyvalues2_noids",
        dm: Datamodel {
            encoding: header.encoding.clone(),
            encoding_version: header.encoding_version,
            format: header.format.clone(),
            format_version: header.format_version,
            ..Datamodel::default()
        },
        guid_seen: HashMap::new(),
        prefix_element: None,
        pending: Vec::new(),
        list_refs: Vec::new(),
        array_serials: HashMap::new(),
        next_serial: 0,
        attr_pos: HashMap::new(),
    };

    while let Some(class_name) = parser.next_token_eof()? {
        parser.parse_element(class_name, 0)?;
    }

    resolve(&mut parser);
    Ok(parser.dm)
}

/// The post-parse pass: element attributes first, then array slots, in upstream's order. A
/// GUID the file never defined resolves to `None`, exactly like upstream
/// `dataModel.AllElements[Guid]` — only an unparsable GUID text is an error.
fn resolve(parser: &mut Parser) {
    for item in std::mem::take(&mut parser.pending) {
        let target = parser.guid_seen.get(&item.guid).copied();
        parser.set_attr(Some(item.owner), &item.attr, Attribute::Element(target));
    }
    for item in std::mem::take(&mut parser.list_refs) {
        let target = parser.guid_seen.get(&item.guid).copied();
        let (holder, name) = match &item.owner {
            ArrayOwner::Element(owner, attr) => (Some(*owner), attr),
            ArrayOwner::Prefix(attr) => (None, attr),
        };
        let key = (holder, name.clone());
        // a slot of an array that a later same-name attribute replaced is dropped, as upstream
        // patches the orphaned list, which nothing holds any more
        if parser.array_serials.get(&key) != Some(&item.serial) {
            continue;
        }
        let Some(&position) = parser.attr_pos.get(&key) else { continue };
        let list = match holder {
            Some(id) => &mut parser.dm.elements[id.0].attributes,
            None => &mut parser.dm.prefix_attributes,
        };
        if let Some(Attribute::ElementArray(items)) = list.get_mut(position).map(|(_, value)| value) {
            if let Some(entry) = items.get_mut(item.index) {
                *entry = target;
            }
        }
    }
    // The prefix element mirrors the prefix attributes, so a model says the same thing whether
    // the encoding that loaded it also writes them as an element (binary 9, keyvalues2 4) or
    // not. This runs last: the arrays above are patched in `prefix_attributes` first.
    if let Some(id) = parser.prefix_element {
        parser.dm.elements[id.0].attributes = parser.dm.prefix_attributes.clone();
    }
}

impl Parser {
    /// Set the named attribute of `holder` (`None`: the prefix attributes): a repeated name keeps
    /// its original position with the last value (upstream's set-through-indexer), a new one is
    /// appended. The position comes from a hash index, so an element with many attributes does
    /// not cost a scan per attribute.
    fn set_attr(&mut self, holder: Option<ElementId>, name: &str, value: Attribute) {
        let list = match holder {
            Some(id) => &mut self.dm.elements[id.0].attributes,
            None => &mut self.dm.prefix_attributes,
        };
        match self.attr_pos.get(&(holder, name.to_string())) {
            Some(&position) => list[position].1 = value,
            None => {
                self.attr_pos.insert((holder, name.to_string()), list.len());
                list.push((name.to_string(), value));
            }
        }
    }

    fn error(&self, message: String) -> Error {
        Error::Decode { at: format!("line {}", self.line), message }
    }

    /// The tokenizer (`Decode_NextToken`): a token is the content of a double-quoted string,
    /// or a single `{` `}` `[` `]`; everything outside quotes is skipped. `None` on actual
    /// end of input — upstream throws `EndOfStreamException` there, which the top level
    /// treats as a clean stop, not an error.
    fn next_token_eof(&mut self) -> Result<Option<String>, Error> {
        let mut out = String::new();
        let mut escaped = false;
        let mut in_block = false;
        while let Some(&c) = self.chars.get(self.pos) {
            self.pos += 1;
            if escaped {
                out.push(match c {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    c => c,
                });
                escaped = false;
                continue;
            }
            match c {
                '"' => {
                    if in_block {
                        return Ok(Some(out));
                    }
                    in_block = true;
                }
                '\\' => escaped = true,
                '\r' | '\n' => {
                    self.line += 1;
                    if in_block {
                        out.push(c);
                    }
                }
                '{' | '}' | '[' | ']' => {
                    if !in_block {
                        return Ok(Some(c.to_string()));
                    }
                    out.push(c);
                }
                c => {
                    if in_block {
                        out.push(c);
                    }
                }
            }
        }
        Ok(None)
    }

    fn next_token(&mut self) -> Result<String, Error> {
        self.next_token_eof()?.ok_or_else(|| {
            self.error("unexpected end of input (a quoted token or structural bracket was missing)".to_string())
        })
    }

    fn guid_or_error(&self, text: &str) -> Result<Guid, Error> {
        text.parse::<Guid>().map_err(|_| self.error(format!("could not parse GUID text {text:?}")))
    }

    /// `Decode_ParseElement`. `class_name` is the class token the caller consumed. The id
    /// line inside `{ … }` builds the arena entry; the `$prefix_element$` block is built at
    /// its opener instead, so it keeps a place in `elements` (with the block's GUID) beside
    /// the folded `prefix_attributes` — see the module docs. In noids every element is built
    /// at its block, since nothing else will (there are no id lines).
    fn parse_element(&mut self, class_name: String, depth: usize) -> Result<Option<ElementId>, Error> {
        // `depth` counts the elements already open, so a file may nest MAX_KV2_DEPTH elements
        // and the writer's own cap lands on the same level
        if depth >= MAX_KV2_DEPTH {
            return Err(self.error(format!(
                "inline element nesting exceeds {MAX_KV2_DEPTH} levels"
            )));
        }
        // every `$prefix_element$` block folds into the prefix attributes, like upstream; the
        // first one also owns the element that stands for the block in `elements`. Later ones
        // only ever set that element's GUID (upstream's `PrefixElementId`), and no prefix block
        // is ever an element a parent could hold.
        let is_prefix_block = class_name == PREFIX_ELEMENT_CLASS;
        let is_first_prefix_block = is_prefix_block && self.prefix_element.is_none();
        let mut elem = if is_first_prefix_block || (self.noids && !is_prefix_block) {
            // upstream folds a prefix block's attributes whatever order its lines come in, so
            // the element cannot wait for the id line; noids elements have no id line at all
            let id = self.new_element("", Guid::default(), &class_name, is_first_prefix_block)?;
            if is_first_prefix_block {
                self.prefix_element = Some(id);
            }
            Some(id)
        } else {
            None
        };
        let mut id_seen = false;
        let mut given_name = String::new();

        match self.next_token()? {
            opener if opener == "{" => {}
            other => {
                return Err(self.error(format!("Expected Element opener, got '{other}'.")));
            }
        }

        loop {
            let attr_name = self.next_token()?;
            if attr_name == "}" {
                break;
            }
            let attr_type_s = self.next_token()?;
            let attr_type = BaseType::of(&attr_type_s);

            // Upstream looks for the id line only while the element does not exist yet, so a
            // second one is an ordinary attribute whose "type" `elementid` is a class name, and
            // fails on the missing opener. A prefix block never has an element and takes every
            // id line (the last wins); a noids element exists from its opener, so its one
            // tolerated id line is tracked by hand.
            let takes_id = is_prefix_block || if self.noids { !id_seen } else { elem.is_none() };
            if attr_name == "id" && attr_type_s == "elementid" && takes_id {
                let id_text = self.next_token()?;
                id_seen = true;
                if is_prefix_block {
                    let guid = self.guid_or_error(&id_text)?;
                    let prefix = self.prefix_element.expect("a prefix block has seen its element");
                    self.dm.elements[prefix.0].id = guid;
                } else if !self.noids {
                    let guid = self.guid_or_error(&id_text)?;
                    elem = Some(self.new_element(&given_name, guid, &class_name, false)?);
                }
                continue;
            }

            if attr_name == "name" && attr_type == BaseType::String {
                given_name = self.next_token()?;
                if let Some(id) = elem.filter(|_| !is_prefix_block) {
                    self.dm.elements[id.0].name = given_name.clone();
                }
                continue;
            }

            if attr_type_s == "element" {
                let id_text = self.next_token()?;
                // Element references need an element to attach to. A prefix block has none in
                // upstream (`HandleElementProp` over a null element rejects a non-empty GUID
                // and drops an empty one), and neither has a block whose own id line has not
                // been read yet. The port's prefix element mirrors `prefix_attributes`, which
                // cannot carry an element, so the same rule applies to it.
                let attachable = elem.filter(|_| !is_prefix_block);
                let Some(owner) = attachable else {
                    if id_text.is_empty() {
                        continue;
                    }
                    return Err(self.error(format!(
                        "element reference for attribute {attr_name:?} has no element to attach to"
                    )));
                };
                self.set_attr(Some(owner), &attr_name, Attribute::Element(None));
                if !id_text.is_empty() {
                    let guid = self.guid_or_error(&id_text)?;
                    self.pending.push(PendingRef { owner, attr: attr_name.clone(), guid });
                }
                continue;
            }

            if attr_type != BaseType::None && attr_type_s.ends_with("_array") {
                let (value, refs) = self.parse_array(attr_type, depth)?;
                self.set_attr(elem.filter(|_| !is_prefix_block), &attr_name, value);
                let holder = elem.filter(|_| !is_prefix_block);
                let serial = self.next_serial;
                self.next_serial += 1;
                self.array_serials.insert((holder, attr_name.clone()), serial);
                for (index, guid) in refs {
                    let owner = match holder {
                        Some(id) => ArrayOwner::Element(id, attr_name.clone()),
                        None => ArrayOwner::Prefix(attr_name.clone()),
                    };
                    self.list_refs.push(PendingListRef { owner, serial, index, guid });
                }
                continue;
            }

            // upstream's `Decode_ParseValue` sends both a null type (an unknown type token,
            // which is an element class name) and the `element` type to `Decode_ParseElement`;
            // for the latter the *value* token is the class name and is trimmed first
            let attr_value = if attr_type == BaseType::None {
                let child = self.parse_element(attr_type_s.clone(), depth + 1)?;
                if child.is_some() && elem.filter(|_| !is_prefix_block).is_none() {
                    // a *defined* element stored as a prefix attribute goes through upstream's
                    // AttributeList setter, which refuses Element values ("Elements are not
                    // supported as prefix attributes."). A nested element with no id of its own
                    // is a null, which upstream stores happily.
                    return Err(self.error(format!(
                        "the inline element of attribute {attr_name:?} cannot become a prefix attribute"
                    )));
                }
                match child {
                    Some(child) => Attribute::Element(Some(child)),
                    None => Attribute::Element(None),
                }
            } else if attr_type == BaseType::Element {
                // a type token like `element_something`: the value token names the class
                let class_name = self.next_token()?;
                match self.parse_element(class_name.trim().to_string(), depth + 1)? {
                    Some(child) => Attribute::Element(Some(child)),
                    None => Attribute::Element(None),
                }
            } else {
                let token = self.next_token()?;
                self.parse_scalar(attr_type, token)?
            };

            self.set_attr(elem.filter(|_| !is_prefix_block), &attr_name, attr_value);
        }
        // a prefix block is not an element a parent can hold (upstream returns null for it)
        Ok(elem.filter(|_| !is_prefix_block))
    }

    /// Build a new arena element and return its id. Mirrors upstream `ElementList.Add`: a
    /// duplicate id is rejected (noids skips this — every GUID there defaults to zero, and so
    /// does a prefix element), and the first element added becomes the root. A prefix element
    /// is neither registered nor ever the root: upstream's `$prefix_element$` block is not an
    /// element at all, and nothing may resolve to it by GUID.
    fn new_element(
        &mut self,
        name: &str,
        guid: Guid,
        class_name: &str,
        prefix: bool,
    ) -> Result<ElementId, Error> {
        if !self.noids && !prefix && self.guid_seen.contains_key(&guid) {
            return Err(self.error(format!(
                "Element ID {} already in use in this Datamodel.",
                guid
            )));
        }
        self.dm
            .elements
            .push(Element {
                class_name: class_name.to_string(),
                name: name.to_string(),
                id: guid,
                stub: false,
                attributes: Vec::new(),
            });
        let id = ElementId(self.dm.elements.len() - 1);
        if !prefix && self.dm.root.is_none() {
            self.dm.root = Some(id);
        }
        if !self.noids && !prefix {
            self.guid_seen.insert(guid, id);
        }
        Ok(id)
    }

    /// The array branch: `[ item item ]`, one token per item. Returns the built attribute
    /// and, for element arrays, the slot index/GUID pairs to resolve once parsing is over.
    fn parse_array(&mut self, attr_type: BaseType, depth: usize) -> Result<(Attribute, Vec<(usize, Guid)>), Error> {
        match self.next_token()? {
            opener if opener == "[" => {}
            other => return Err(self.error(format!("Expected array opener, got '{other}'."))),
        }
        if attr_type == BaseType::Element {
            // element slots: a GUID fills its slot in after the whole file parsed, which is
            // why `None` stands in for each reference while parsing
            let mut slots: Vec<Option<ElementId>> = Vec::new();
            let mut refs: Vec<(usize, Guid)> = Vec::new();
            loop {
                let item_token = self.next_token()?;
                if item_token == "]" {
                    break;
                }
                if item_token == "element" {
                    let id_text = self.next_token()?;
                    let index = slots.len();
                    slots.push(None);
                    if !id_text.is_empty() {
                        let guid = self.guid_or_error(&id_text)?;
                        refs.push((index, guid));
                    }
                } else {
                    // an inline element: the class name came as the item's token
                    let child = self.parse_element(item_token, depth + 1)?;
                    slots.push(child);
                }
            }
            return Ok((Attribute::ElementArray(slots), refs));
        }
        let mut buffer = ValueBuf::new(attr_type)?;
        loop {
            let item_token = self.next_token()?;
            if item_token == "]" {
                break;
            }
            if item_token == "element" {
                // upstream reads this as an id reference and casts the array to `ElementArray`
                return Err(self.error("an element reference in an array of values".to_string()));
            }
            buffer.push(self.parse_scalar(attr_type, item_token)?)?;
        }
        Ok((buffer.into_attribute(), Vec::new()))
    }

    /// `Decode_ParseValue` for one scalar attribute.
    fn parse_scalar(&mut self, attr_type: BaseType, raw: String) -> Result<Attribute, Error> {
        if attr_type == BaseType::String {
            return Ok(Attribute::String(raw)); // strings keep their whitespace as-is
        }
        let value = raw.trim(); // upstream trims once for every other type
        Ok(match attr_type {
            BaseType::Int => Attribute::Int(value
                .parse::<i32>()
                .map_err(|e| self.error(format!("invalid int {value:?}: {e}")))?),
            BaseType::Float => Attribute::Float(parse_float(value)
                .map_err(|e| self.error(format!("invalid float {value:?}: {e}")))?),
            BaseType::Bool => Attribute::Bool(
                // upstream: byte.Parse(value) == 1, so "0" and every other byte yields false
                parse_byte(value).map_err(|e| self.error(format!("invalid bool {value:?}: {e}")))? == 1,
            ),
            BaseType::Binary => Attribute::Binary(parse_binary(self, value)?),
            BaseType::Time => {
                // upstream: TimeSpan.FromTicks((long)(seconds * TimeSpan.TicksPerSecond)) —
                // the same truncation, with the cast saturating where C# would be UB
                let seconds = parse_double(value)
                    .map_err(|e| self.error(format!("invalid time {value:?}: {e}")))?;
                let ticks = (seconds * 10_000_000.0) as i64;
                Attribute::Time(Time(ticks as f64 / 10_000_000.0))
            }
            BaseType::Color => Attribute::Color(parse_color(self, value)?),
            BaseType::Vector2 => {
                let floats = take_floats(self, value, 2)?;
                Attribute::Vector2([floats[0], floats[1]])
            }
            BaseType::Vector3 => {
                let floats = take_floats(self, value, 3)?;
                Attribute::Vector3([floats[0], floats[1], floats[2]])
            }
            BaseType::Vector4 => {
                let floats = take_floats(self, value, 4)?;
                Attribute::Vector4([floats[0], floats[1], floats[2], floats[3]])
            }
            BaseType::Quaternion => {
                let floats = take_floats(self, value, 4)?;
                Attribute::Quaternion([floats[0], floats[1], floats[2], floats[3]])
            }
            BaseType::QAngle => {
                // qangle shares the numeral shape with vector3; upstream distinguishes them
                // only by type, and the version-4 type list is what admits a qangle
                let floats = take_floats(self, value, 3)?;
                Attribute::QAngle([floats[0], floats[1], floats[2]])
            }
            BaseType::Matrix => Attribute::Matrix(take_matrix(self, value)?),
            BaseType::UInt8 => Attribute::Byte(parse_byte(value)
                .map_err(|e| self.error(format!("invalid uint8 {value:?}: {e}")))?),
            BaseType::UInt64 => {
                // upstream: `ulong.Parse(value.Remove(0, 2), NumberStyles.HexNumber)`. Remove drops
                // two UTF-16 *units* whatever they are (a keyvalues2 uint64 carries "0x" only by
                // convention), and HexNumber then takes optional white space around bare hex
                // digits: no sign, no second "0x".
                let mut units = 0;
                let mut cut = 0;
                for c in value.chars() {
                    if units >= 2 {
                        break;
                    }
                    units += c.len_utf16();
                    cut += c.len_utf8();
                }
                if units != 2 {
                    // too short for Remove(0, 2), or the cut lands inside a surrogate pair
                    return Err(self.error(format!("uint64 must carry a 0x prefix, got {value:?}")));
                }
                let hex = value[cut..].trim_matches(dotnet_white);
                if hex.starts_with('+') {
                    return Err(self.error(format!("invalid uint64 {value:?}: a sign is not a hex digit")));
                }
                Attribute::UInt64(u64::from_str_radix(hex, 16)
                    .map_err(|e| self.error(format!("invalid uint64 {value:?}: {e}")))?)
            }
            BaseType::None | BaseType::Element | BaseType::String => {
                return Err(self.error("ParseValue passed an element-type value".to_string()));
            }
        })
    }
}

/// The white space .NET's number parsing skips around a value: U+0009..U+000D and U+0020.
fn dotnet_white(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ')
}

/// `byte.Parse(text, InvariantCulture)`: an optional sign, then digits; a minus sign is only
/// accepted in front of zeros (`-0` is 0, `-1` overflows), where Rust refuses any minus.
fn parse_byte(text: &str) -> Result<u8, std::num::ParseIntError> {
    match text.strip_prefix('-') {
        Some(zeros) if !zeros.is_empty() && zeros.bytes().all(|digit| digit == b'0') => Ok(0),
        _ => text.parse::<u8>(),
    }
}

/// `float.Parse(text, InvariantCulture)`: Rust's grammar, minus the spellings .NET lacks. `inf`
/// is not a float to .NET (`Infinity` and `NaN` are, in any case), and digit group separators
/// (`1,000`) are accepted by .NET but not here.
fn parse_float(text: &str) -> Result<f32, std::num::ParseFloatError> {
    text.parse::<f32>().and_then(|value| not_bare_inf(text, value))
}

/// The `f64` twin of [`parse_float`], for `time`.
fn parse_double(text: &str) -> Result<f64, std::num::ParseFloatError> {
    text.parse::<f64>().and_then(|value| not_bare_inf(text, value))
}

/// Refuse the one spelling Rust reads as infinity that .NET does not: `inf`.
fn not_bare_inf<T>(text: &str, value: T) -> Result<T, std::num::ParseFloatError> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned.eq_ignore_ascii_case("inf") {
        // Rust's own error value for a non-number
        return Err("inf!".parse::<f32>().expect_err("not a number"));
    }
    Ok(value)
}

/// Upstream's binary parse: backslashes, CR, LF, tabs and spaces are stripped first (Hammer
/// wraps its binary export with two tabs), then hex pairs are read.
fn parse_binary(parser: &Parser, value: &str) -> Result<Vec<u8>, Error> {
    let cleaned: String = value
        .chars()
        .filter(|c| !matches!(c, '\\' | '\r' | '\n' | '\t' | ' '))
        .collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(parser.error("a binary value needs an even number of hex digits".to_string()));
    }
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().as_chunks::<2>().0 {
        let byte = hex_pair(pair)
            .ok_or_else(|| parser.error(format!("invalid hex pair {pair:?} in a binary value")))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Upstream's color parse: four byte values, `Color.FromBytes`-style (extra numbers are read
/// to keep the throw-on-garbage behavior, then dropped).
fn parse_color(parser: &Parser, value: &str) -> Result<[u8; 4], Error> {
    let mut color = [0u8; 4];
    let mut parts = value.split_whitespace();
    for slot in &mut color {
        let part = parts
            .next()
            .ok_or_else(|| parser.error("a color needs four byte values".to_string()))?;
        *slot = parse_byte(part)
            .map_err(|e| parser.error(format!("invalid color byte {part:?}: {e}")))?;
    }
    // a longer row still has to consist of bytes, like .NET's byte.Parse over every part
    for extra in parts.by_ref() {
        parse_byte(extra)
            .map_err(|e| parser.error(format!("invalid color byte {extra:?}: {e}")))?;
    }
    Ok(color)
}

/// Upstream's vector-family parse (`num_list` over the split columns, `float.Parse` per
/// column): every part must parse, at least `n` of them are needed, the rest are dropped.
fn take_floats(parser: &Parser, value: &str, n: usize) -> Result<Vec<f32>, Error> {
    let mut floats = Vec::with_capacity(n + 4);
    for part in value.split_whitespace() {
        floats.push(parse_float(part)
            .map_err(|e| parser.error(format!("invalid float {part:?}: {e}")))?);
    }
    if floats.len() < n {
        return Err(parser.error(format!("expected {n} floats, found {}", floats.len())));
    }
    Ok(floats)
}

fn take_matrix(parser: &Parser, value: &str) -> Result<[[f32; 4]; 4], Error> {
    let floats = take_floats(parser, value, 16)?;
    Ok([
        [floats[0], floats[1], floats[2], floats[3]],
        [floats[4], floats[5], floats[6], floats[7]],
        [floats[8], floats[9], floats[10], floats[11]],
        [floats[12], floats[13], floats[14], floats[15]],
    ])
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// `KV2Writer`: the output bytes plus the current indentation.
struct Writer {
    out: Vec<u8>,
    indent: usize,
}

impl Writer {
    /// One full line: the current indentation, the text, LF.
    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.out.push(b'\t');
        }
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(b'\n');
    }

    /// A bare blank line (like upstream `KV2Writer.WriteLine()`: no indentation).
    fn blank(&mut self) {
        self.out.push(b'\n');
    }
}

/// The encoder's mutable state, kept apart from `Datamodel` so attribute reads and writer
/// writes never borrow-conflict.
struct Encoder<'a> {
    dm: &'a Datamodel,
    layout: Layout,
    writer: Writer,
    supports: bool,
    /// Reference counts, filled by the walk from the root (upstream `ReferenceCount`).
    counts: HashMap<ElementId, u32>,
    /// Elements in first-seen order, so [`Layout::Upstream`] writes the after-root blocks in
    /// upstream's order.
    order: Vec<ElementId>,
    depth: usize,
}

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

/// How the writer lays a keyvalues2 file out.
///
/// The two layouts differ in the two places the text format leaves a writer free: the order of
/// the element blocks after the root, and the shape of a long `binary` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// What Valve's tools write (`dmxconvert`, Hammer's text export), and the layout of the two
    /// real `.vmap.txt` files the tests use: the blocks after the root come in
    /// [`Datamodel::elements`] order — the order the file defines them in, which is the order a
    /// reader builds the arena in — and a `binary` value longer than one line of hex is wrapped
    /// ([`BINARY_WRAP_COLUMNS`]). This is [`encode`], and so [`crate::save`], because it is what
    /// makes a Valve file come back byte for byte.
    #[default]
    Valve,
    /// What upstream's `KeyValues2.cs` writes: the blocks after the root in the order the
    /// reference-counting walk first reaches them, and every `binary` value on the attribute
    /// line, however long. The upstream C# tests pin this layout.
    Upstream,
}

/// The number of hex digits on one line of a wrapped `binary` value — what Valve's writer emits
/// (40 bytes), the only width the sample file shows.
pub const BINARY_WRAP_COLUMNS: usize = 80;

/// Encode a whole keyvalues2 / keyvalues2_noids file in [`Layout::Valve`], header included: what
/// [`crate::save`] writes, and what reproduces a file Valve's tools wrote.
pub fn encode(dm: &Datamodel) -> Result<Vec<u8>, Error> {
    encode_with(dm, Layout::Valve)
}

/// Encode a whole keyvalues2 / keyvalues2_noids file in the layout asked for, header included.
pub fn encode_with(dm: &Datamodel, layout: Layout) -> Result<Vec<u8>, Error> {
    // the same table `save` dispatches by: this codec writes keyvalues2 and noids, versions 1-4
    if !matches!(dm.encoding.as_str(), "keyvalues2" | "keyvalues2_noids") || !(1..=4).contains(&dm.encoding_version) {
        return Err(Error::Unsupported { encoding: dm.encoding.clone(), version: dm.encoding_version });
    }
    validate_references(dm)?;
    let mut encoder = Encoder {
        dm,
        layout,
        writer: Writer { out: Vec::new(), indent: 0 },
        supports: dm.encoding != "keyvalues2_noids",
        counts: HashMap::new(),
        order: Vec::new(),
        depth: 0,
    };
    encoder.run()?;
    Ok(encoder.writer.out)
}

/// Every element the encoder might reach must be inside the arena; a hand-built model may
/// carry anything, but the writer would panic hunting for it. A prefix attribute that *is* an
/// element is refused too: upstream's `AttributeList` never stores one there ("Elements are not
/// supported as prefix attributes"), so its writer never meets one, while this one would write
/// a reference the prefix block cannot hold.
fn validate_references(dm: &Datamodel) -> Result<(), Error> {
    if dm
        .prefix_attributes
        .iter()
        .any(|(_, attr)| matches!(attr, Attribute::Element(Some(_))))
    {
        return Err(Error::Encode("elements are not supported as prefix attributes".to_string()));
    }
    let inside = |id: Option<ElementId>| id.is_none_or(|child| child.0 < dm.elements.len());
    if !inside(dm.root) {
        return Err(Error::Encode("the root points outside the element list".to_string()));
    }
    for element in &dm.elements {
        for (_, attr) in &element.attributes {
            match attr {
                Attribute::Element(target) if !inside(*target) => {
                    return Err(Error::Encode(
                        "an element reference points outside the element list".to_string(),
                    ));
                }
                Attribute::ElementArray(items) if items.iter().any(|item| !inside(*item)) => {
                    return Err(Error::Encode(
                        "an element array holds a reference outside the element list".to_string(),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

impl Encoder<'_> {
    fn run(&mut self) -> Result<(), Error> {
        let dm = self.dm;
        self.writer.line(&format_header(
            &dm.encoding,
            dm.encoding_version,
            &dm.format,
            dm.format_version,
        ));

        // The prefix block, written once, from `prefix_attributes` (so an edit to them shows).
        // Only the block's GUID comes from the prefix element, the element that stands for it
        // in `elements`; a model without one gets the all-zero GUID, the frozen model has no
        // `PrefixElementId`. Upstream writes the block only when there are prefix attributes;
        // the Valve layout also keeps a block that carries nothing but its id line, which a
        // file can hold.
        let prefix = dm.prefix_element();
        let has_block = match self.layout {
            Layout::Valve => prefix.is_some() || !dm.prefix_attributes.is_empty(),
            Layout::Upstream => !dm.prefix_attributes.is_empty(),
        };
        if dm.encoding_version >= 4 && has_block {
            let guid = prefix.map(|id| dm.elements[id.0].id).unwrap_or_default();
            self.writer.line(&token(PREFIX_ELEMENT_CLASS));
            self.writer.line("{");
            self.writer.indent += 1;
            self.writer.line(&format!("\"id\" \"elementid\" \"{guid}\""));
            for (name, attr) in &dm.prefix_attributes {
                self.write_attribute(name, attr)?;
            }
            self.writer.indent -= 1;
            self.writer.line("}");
        }

        if self.supports {
            self.count_references(dm.root);
            self.check_unique_ids(prefix)?;
        }

        // the prefix element is never written as an element: it is the block above
        if let Some(root) = dm.root.filter(|root| Some(*root) != prefix) {
            self.write_element(root)?;
            // a blank line follows the root, and every block after gets one too
            self.writer.blank();
        }

        if self.supports {
            // the blocks after the root: Valve's writer takes the elements in the order the file
            // carried them (a reader builds the arena in that order), upstream's in the order
            // its reference walk met them
            let ids: Vec<ElementId> = match self.layout {
                Layout::Valve => (0..dm.elements.len()).map(ElementId).collect(),
                Layout::Upstream => self.order.clone(),
            };
            for id in ids {
                if Some(id) == prefix || self.counts.get(&id).copied().unwrap_or(0) <= 1 {
                    continue;
                }
                if Some(id) != dm.root {
                    self.write_element(id)?;
                    self.writer.blank();
                }
            }
        }
        Ok(())
    }

    /// The ids are what a reference names and what the decoder keys elements by, so two
    /// reachable elements sharing one cannot be written as a file that reads back (the decoder
    /// refuses "already in use"). The prefix element's id is the prefix block's, not an
    /// element's.
    fn check_unique_ids(&self, prefix: Option<ElementId>) -> Result<(), Error> {
        let mut seen: HashMap<Guid, ElementId> = HashMap::new();
        for id in self.order.iter().filter(|id| Some(**id) != prefix) {
            let guid = self.dm.elements[id.0].id;
            if let Some(first) = seen.insert(guid, *id) {
                return Err(Error::Encode(format!(
                    "elements {} and {} share the id {guid}",
                    first.0, id.0
                )));
            }
        }
        Ok(())
    }

    /// Upstream `CountReferences`, flattened: the count rises once per reference met, but the
    /// attribute walk descends only on the first visit — neither half of the upstream
    /// recursion runs on the stack any more.
    fn count_references(&mut self, root: Option<ElementId>) {
        let Some(root) = root else { return };
        let mut stack: Vec<(ElementId, Vec<ElementId>, usize)> = Vec::new();
        if ref_visit(&mut self.counts, root) {
            self.order.push(root);
            stack.push((root, element_children(self.dm, root), 0));
        }
        while !stack.is_empty() {
            let step = {
                let (_, children, cursor) = stack.last_mut().expect("stack has entries");
                let taken = children.get(*cursor).copied();
                *cursor += 1;
                taken
            };
            match step {
                Some(child) => {
                    if ref_visit(&mut self.counts, child) {
                        self.order.push(child);
                        stack.push((child, element_children(self.dm, child), 0));
                    }
                }
                None => {
                    stack.pop();
                }
            }
        }
    }

    /// `"ClassName" { … }` — a top-level block.
    fn write_element(&mut self, id: ElementId) -> Result<(), Error> {
        {
            let class_name = self.dm.elements[id.0].class_name.clone();
            self.writer.line(&token(&class_name));
        }
        self.write_element_body(id)?;
        self.writer.line("}");
        Ok(())
    }

    /// `{`, id, name, attributes; the caller writes the class name before and `}` after.
    fn write_element_body(&mut self, id: ElementId) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_KV2_DEPTH {
            self.depth -= 1;
            return Err(Error::Encode(format!(
                "inline element nesting exceeds {MAX_KV2_DEPTH} levels"
            )));
        }
        {
            let element = &self.dm.elements[id.0];
            if RESERVED_TYPE_NAMES.contains(&element.class_name.as_str()) {
                return Err(Error::Encode(format!(
                    "Element {} uses reserved type name \"{}\"",
                    element.id,
                    element.class_name.clone()
                )));
            }
        }
        self.writer.line("{");
        self.writer.indent += 1;
        {
            let guid = self.dm.elements[id.0].id;
            if self.supports {
                self.writer.line(&format!("\"id\" \"elementid\" \"{}\"", guid));
            }
            let name = self.dm.elements[id.0].name.clone();
            if !name.is_empty() {
                self.writer.line(&format!("\"name\" \"string\" {}", token(&name)));
            }
        }
        // the clone is what keeps the write loop borrow-free against the read pass upstream
        // serializes from an attribute cache of its own
        let attrs: Vec<(String, Attribute)> = self.dm.elements[id.0].attributes.clone();
        for (name, attr) in &attrs {
            self.write_attribute(name, attr)?;
        }
        self.writer.indent -= 1;
        self.depth -= 1;
        Ok(())
    }

    fn write_attribute(&mut self, name: &str, attr: &Attribute) -> Result<(), Error> {
        let name_token = token(name);
        match attr {
            Attribute::Element(child) => self.write_element_attribute(&name_token, *child),
            _ => match array_inner_type(attr) {
                Some(inner) => {
                    if !valid_in_version(inner, self.dm.encoding_version) {
                        return Err(Error::Encode(format!(
                            "{inner} is not valid in KeyValues2 {}",
                            self.dm.encoding_version
                        )));
                    }
                    match attr {
                        Attribute::ElementArray(items) => {
                            self.write_reference_array(&name_token, inner, items)
                        }
                        _ => self.write_value_array(&name_token, inner, attr),
                    }
                }
                None => {
                    let Some(type_name) = scalar_type_name(attr) else {
                        return Err(Error::Encode(format!(
                            "Cannot serialize a value of type {} to KeyValues2",
                            attr_variant_name(attr)
                        )));
                    };
                    if !valid_in_version(type_name, self.dm.encoding_version) {
                        return Err(Error::Encode(format!(
                            "{type_name} is not valid in KeyValues2 {}",
                            self.dm.encoding_version
                        )));
                    }
                    // a long binary value is wrapped in Valve's layout, one line per
                    // [`BINARY_WRAP_COLUMNS`] hex digits; upstream puts every length on one line
                    if let Attribute::Binary(bytes) = attr {
                        if self.layout == Layout::Valve && bytes.len() * 2 > BINARY_WRAP_COLUMNS {
                            return self.write_wrapped_binary(&name_token, type_name, bytes);
                        }
                    }
                    let value = format_value(attr)?;
                    self.writer
                        .line(&format!("{name_token} {} {}", token(type_name), token(&value)));
                    Ok(())
                }
            },
        }
    }

    /// `{name} "element" "guid"`; elements referenced more than once (when ids are supported)
    /// print as id references, everything else as an inline `{ … }` block plus a blank line.
    /// A null reference prints an empty GUID.
    fn write_element_attribute(&mut self, name_token: &str, child: Option<ElementId>) -> Result<(), Error> {
        if child.is_none() || self.should_be_referenced(child) {
            let id_text = child
                .map(|id| self.dm.elements[id.0].id.to_string())
                .unwrap_or_default();
            self.writer.line(&format!("{name_token} \"element\" \"{id_text}\""));
            return Ok(());
        }
        let child = child.expect("the None case is handled above");
        {
            let class_name = self.dm.elements[child.0].class_name.clone();
            self.writer.line(&format!("{name_token} {}", token(&class_name)));
        }
        self.write_element_body(child)?;
        // the reference serializer leaves a blank line after an inline element
        self.writer.line("}");
        self.writer.blank();
        Ok(())
    }

    fn should_be_referenced(&self, elem: Option<ElementId>) -> bool {
        self.supports
            && elem
                .and_then(|id| self.counts.get(&id).copied())
                .is_some_and(|count| count > 1)
    }

    fn write_reference_array(
        &mut self,
        name_token: &str,
        inner: &'static str,
        items: &[Option<ElementId>],
    ) -> Result<(), Error> {
        self.writer
            .line(&format!("{name_token} {} ", token(&format!("{inner}_array"))));
        self.writer.line("[");
        self.writer.indent += 1;
        let count = items.len();
        for (index, item) in items.iter().enumerate() {
            let separator = if index == count - 1 { "" } else { "," };
            if item.is_none() || self.should_be_referenced(*item) {
                let id_text = item
                    .map(|id| self.dm.elements[id.0].id.to_string())
                    .unwrap_or_default();
                self.writer.line(&format!("\"element\" \"{id_text}\"{separator}"));
            } else {
                let item = item.expect("the None case is handled above");
                {
                    let class_name = self.dm.elements[item.0].class_name.clone();
                    self.writer.line(&token(&class_name));
                }
                self.write_element_body(item)?;
                self.writer.line(&format!("}}{separator}"));
            }
        }
        self.writer.indent -= 1;
        self.writer.line("]");
        Ok(())
    }

    /// Hammer's readable `binary` layout (see [`Layout::Valve`]): the name and type close their
    /// line, the value opens on a line of its own, the hex runs [`BINARY_WRAP_COLUMNS`] digits
    /// per line one indent deeper, and the closing quote returns to the attribute's own
    /// indentation. The line breaks and tabs are *inside* the quoted token; the decoder strips
    /// them off a binary value again, which is why the reference reader sanitises them.
    fn write_wrapped_binary(
        &mut self,
        name_token: &str,
        type_name: &str,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let hex = hex_string(bytes);
        self.writer.line(&format!("{name_token} {} ", token(type_name)));
        self.writer.line("\"");
        self.writer.indent += 1;
        for chunk in hex.as_bytes().chunks(BINARY_WRAP_COLUMNS) {
            let text = std::str::from_utf8(chunk).expect("hex digits are ASCII");
            self.writer.line(text);
        }
        self.writer.indent -= 1;
        self.writer.line("\"");
        Ok(())
    }

    fn write_value_array(
        &mut self,
        name_token: &str,
        inner: &'static str,
        array: &Attribute,
    ) -> Result<(), Error> {
        self.writer
            .line(&format!("{name_token} {} ", token(&format!("{inner}_array"))));
        self.writer.line("[");
        self.writer.indent += 1;
        let count = item_count(array);
        for index in 0..count {
            let separator = if index == count - 1 { "" } else { "," };
            let text = format_array_item(array, index)?;
            self.writer.line(&format!("{}{separator}", token(&text)));
        }
        self.writer.indent -= 1;
        self.writer.line("]");
        Ok(())
    }
}

fn item_count(array: &Attribute) -> usize {
    match array {
        Attribute::ElementArray(items) => items.len(),
        Attribute::IntArray(items) => items.len(),
        Attribute::FloatArray(items) => items.len(),
        Attribute::BoolArray(items) => items.len(),
        Attribute::StringArray(items) => items.len(),
        Attribute::BinaryArray(items) => items.len(),
        Attribute::TimeArray(items) => items.len(),
        Attribute::ColorArray(items) => items.len(),
        Attribute::Vector2Array(items) => items.len(),
        Attribute::Vector3Array(items) => items.len(),
        Attribute::Vector4Array(items) => items.len(),
        Attribute::QAngleArray(items) => items.len(),
        Attribute::QuaternionArray(items) => items.len(),
        Attribute::MatrixArray(items) => items.len(),
        Attribute::UInt64Array(items) => items.len(),
        Attribute::ByteArray(items) => items.len(),
        _ => 0,
    }
}

/// The children an element references, for the counting walk.
fn element_children(dm: &Datamodel, id: ElementId) -> Vec<ElementId> {
    let element = &dm.elements[id.0];
    let mut children = Vec::new();
    for (_, attr) in &element.attributes {
        match attr {
            Attribute::Element(Some(child)) => children.push(*child),
            Attribute::ElementArray(items) => children.extend(items.iter().flatten().copied()),
            _ => {}
        }
    }
    children
}

/// Push one visit: `true` when this element had not been met before, so the walk may descend
/// into its attributes.
#[must_use]
fn ref_visit(counts: &mut HashMap<ElementId, u32>, id: ElementId) -> bool {
    let entry = counts.entry(id).or_insert(0);
    *entry += 1;
    *entry == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The f64 rows of `tests/data/float64_format_table.txt` (generated with .NET 10 by
    /// `tests/dotnet/FloatTable`): exact 15-digit ties, which no f32 can produce, huge integers,
    /// random doubles.
    #[test]
    fn format_float_matches_dotnet_for_doubles() {
        let table = include_str!("../../tests/data/float64_format_table.txt");
        let mut rows = 0;
        for line in table.lines() {
            let (bits, text) = line.split_once(' ').expect("a bits/text row");
            let value = f64::from_bits(u64::from_str_radix(bits, 16).unwrap());
            assert_eq!(format_float(value), text, "bits {bits}");
            rows += 1;
        }
        assert!(rows >= 1000, "{rows} rows");
    }
}
