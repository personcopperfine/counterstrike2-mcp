//! `cvar.get`: read console variables from the engine's `ICvar` (bridge m3.13).
//!
//! `cvar.get {names:[string] (1..64)}` answers `{"cvars":[{name, found, type, value, default,
//! flags:[..], flagsRaw}, ...]}`, one entry per requested name, in order. A name the engine does
//! not know (or a devonly cvar, which `FindConVar(name, false)` hides exactly like the in-game
//! console) is `{name, found:false}`.
//!
//! # The engine calls (RE: `workflows/streamline/re/cvar-re.md`)
//!
//! * the object is `VEngineCvar007` from tier0 (`s2sdk::module::versions::CVAR_INTERFACE_VERSION`),
//!   held to the pinned `CCvar` vtable and its two called slots ([`pins::T0V_CCVAR_VT`]) by
//!   [`obj_is`] like every other engine object;
//! * `ICvar::FindConVar` (generated slot, binary-verified by a sigs `sdk` row) returns the 8-byte
//!   `ConVarRef` through the MSVC hidden return pointer (the generated Windows signature spells
//!   `ret`); an access index of `0xFFFF` is "not found";
//! * `ICvar::GetConVarData` takes that `ConVarRef` by value and answers the `ConVarData`;
//! * the members read are the generated `ConVarData` fields `m_eVarType`, `m_nFlags`,
//!   `m_defaultValue` and `m_Values` (slot 0 is the live value), each held to tier0.dll by a sigs
//!   `sdk` row; the flag names are the generated `FCVAR_*` consts.
//!
//! # Values
//!
//! `value` is the text the engine's own `ValueToString` prints (bool `true`/`false`, integers in
//! decimal, floats in the shortest round-trip form, a colour as `r g b` or `r g b a`, vectors
//! space-separated). A `FCVAR_PROTECTED` cvar (passwords) is reported with `value:null,
//! default:null, redacted:true`. The op runs on the main thread (the queue drain), which is what
//! makes reading a string value's `CUtlString` safe.

use core::ffi::{c_char, c_void, CStr};
use core::ptr::addr_of;

use s2sdk_sys::generated as sys;
use serde_json::{json, Map, Value};

use crate::control;
use crate::generated::pins;
use crate::ops::ident::{mem_ok, mod_iface, obj_is};
use crate::ops::{Handler, OpErr};
use crate::queue::Request;

/// The method table of this group.
pub static HANDLERS: &[(&str, Handler)] = &[("cvar.get", cvar_get)];

/// `names` holds 1..=64 entries.
pub const NAMES_MAX: usize = 64;
/// One name's longest accepted form.
pub const NAME_MAX_BYTES: usize = 128;
/// A string value is cut at this many bytes (`valueTruncated:true`).
pub const VALUE_MAX_BYTES: usize = 1024;
/// `ConVarRef::kInvalidAccessIndex` (`tier0/convar.h`): the access index of "not found". Generated
/// (a u32 const: gen.py's consts table has no u16), held to tier0.dll by a sigs `sdk` row.
const INVALID_ACCESS_INDEX: u16 = sys::CONVARREF_INVALID_ACCESS_INDEX as u16;
const _: () = assert!(sys::CONVARREF_INVALID_ACCESS_INDEX <= u16::MAX as u32);

// `CVValue_t` is a 16-byte union whose members all start at offset 0; the decoder reads the 16
// bytes and takes each member from the front.
const _: () = assert!(core::mem::size_of::<sys::CVValue_t>() == 16);

/// The flag bits that get a name, in report order: the generated `FCVAR_*` consts.
const FLAG_NAMES: &[(u64, &str)] = &[
    (sys::FCVAR_DEVELOPMENTONLY, "devonly"),
    (sys::FCVAR_HIDDEN, "hidden"),
    (sys::FCVAR_PROTECTED, "protected"),
    (sys::FCVAR_ARCHIVE, "archive"),
    (sys::FCVAR_NOTIFY, "notify"),
    (sys::FCVAR_REPLICATED, "replicated"),
    (sys::FCVAR_CHEAT, "cheat"),
    (sys::FCVAR_RELEASE, "release"),
];

/// The named flags set in `flags`.
#[must_use]
pub fn flag_names(flags: u64) -> Vec<&'static str> {
    FLAG_NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// The engine's name for a `m_eVarType` value (tier0's type-traits table, same order as the
/// generated `EConVarType`), `"unknown"` for anything else.
#[must_use]
pub fn type_name(raw: i16) -> &'static str {
    use sys::EConVarType as T;
    match T(raw) {
        T::EConVarType_Bool => "bool",
        T::EConVarType_Int16 => "int16",
        T::EConVarType_UInt16 => "uint16",
        T::EConVarType_Int32 => "int32",
        T::EConVarType_UInt32 => "uint32",
        T::EConVarType_Int64 => "int64",
        T::EConVarType_UInt64 => "uint64",
        T::EConVarType_Float32 => "float32",
        T::EConVarType_Float64 => "float64",
        T::EConVarType_String => "string",
        T::EConVarType_Color => "color",
        T::EConVarType_Vector2 => "vector2",
        T::EConVarType_Vector3 => "vector3",
        T::EConVarType_Vector4 => "vector4",
        T::EConVarType_Qangle => "qangle",
        T::EConVarType_VectorWS => "vectorws",
        _ => "unknown",
    }
}

/// Validate `params` and return the names. `Err` is the `InvalidParams` reason.
pub fn parse_names(params: Option<&str>) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(params.unwrap_or("{}"))
        .map_err(|_| "params must be a JSON object".to_string())?;
    let list = value
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| "names (array of strings) required".to_string())?;
    if list.is_empty() || list.len() > NAMES_MAX {
        return Err(format!("names must hold 1..={NAMES_MAX} entries, got {}", list.len()));
    }
    list.iter()
        .map(|item| {
            item.as_str()
                .filter(|s| {
                    !s.is_empty() && s.len() <= NAME_MAX_BYTES && !s.chars().any(char::is_control)
                })
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "every name must be a non-empty string of at most {NAME_MAX_BYTES} bytes \
                         without control characters"
                    )
                })
        })
        .collect()
}

fn f32_at(bytes: &[u8; 16], index: usize) -> f32 {
    f32::from_le_bytes([bytes[index * 4], bytes[index * 4 + 1], bytes[index * 4 + 2], bytes[index * 4 + 3]])
}

fn floats(bytes: &[u8; 16], count: usize) -> String {
    (0..count).map(|i| f32_at(bytes, i).to_string()).collect::<Vec<_>>().join(" ")
}

/// The text of the value in `bytes` (a `CVValue_t`'s 16 bytes) for `var_type`; `None` for a type
/// this decoder does not know. A string's pointer is followed only when readable; it is cut at
/// [`VALUE_MAX_BYTES`] (second field: it was cut).
#[must_use]
pub fn value_text(var_type: i16, bytes: &[u8; 16]) -> Option<(String, bool)> {
    use sys::EConVarType as T;
    let u64_at = || u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let text = match T(var_type) {
        T::EConVarType_Bool => (bytes[0] != 0).to_string(),
        T::EConVarType_Int16 => i16::from_le_bytes([bytes[0], bytes[1]]).to_string(),
        T::EConVarType_UInt16 => u16::from_le_bytes([bytes[0], bytes[1]]).to_string(),
        T::EConVarType_Int32 => i32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")).to_string(),
        T::EConVarType_UInt32 => u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")).to_string(),
        T::EConVarType_Int64 => (u64_at() as i64).to_string(),
        T::EConVarType_UInt64 => u64_at().to_string(),
        T::EConVarType_Float32 => f32_at(bytes, 0).to_string(),
        T::EConVarType_Float64 => f64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")).to_string(),
        T::EConVarType_String => {
            let pointer = u64_at() as usize as *const c_char;
            if pointer.is_null() {
                String::new()
            } else if !mem_ok(pointer.cast::<c_void>()) {
                return None;
            } else {
                // SAFETY: `mem_ok` proved the first byte readable; the string is the
                // `CUtlString`'s NUL-terminated buffer, owned by the game thread, which is the
                // thread this op runs on (module docs).
                let raw = unsafe { CStr::from_ptr(pointer) }.to_bytes();
                let cut = raw.len() > VALUE_MAX_BYTES;
                let shown = &raw[..raw.len().min(VALUE_MAX_BYTES)];
                return Some((String::from_utf8_lossy(shown).into_owned(), cut));
            }
        }
        T::EConVarType_Color => {
            let [r, g, b, a] = [bytes[0], bytes[1], bytes[2], bytes[3]];
            if a == 255 {
                format!("{r} {g} {b}")
            } else {
                format!("{r} {g} {b} {a}")
            }
        }
        T::EConVarType_Vector2 => floats(bytes, 2),
        T::EConVarType_Vector3 | T::EConVarType_Qangle | T::EConVarType_VectorWS => floats(bytes, 3),
        T::EConVarType_Vector4 => floats(bytes, 4),
        _ => return None,
    };
    Some((text, false))
}

/// What a lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The engine has no (visible) cvar of that name.
    NotFound,
    /// `FindConVar` found it but `GetConVarData` answered null.
    NoData,
    /// The cvar's data record.
    Found(*const sys::ConVarData),
}

/// Where the lookups come from: the engine in production, a synthetic record in the host tests.
pub trait CvarSource {
    /// Look `name` up.
    fn lookup(&self, name: &str) -> Lookup;
}

/// The 16 bytes of the `CVValue_t` at `value`, `None` when unreadable.
///
/// # Safety
///
/// `value` is null or points at memory the engine owns (a `CVValue_t`).
unsafe fn read_value_bytes(value: *const sys::CVValue_t) -> Option<[u8; 16]> {
    if value.is_null() || !mem_ok(value.cast::<c_void>()) {
        return None;
    }
    // SAFETY: the first byte is readable and a `CVValue_t` is 16 bytes (the const assert above);
    // `read_unaligned` of plain bytes cannot be UB.
    Some(unsafe { value.cast::<[u8; 16]>().read_unaligned() })
}

/// One found cvar's reply entry.
///
/// # Safety
///
/// `data` points at a live `ConVarData` (the engine's record from `GetConVarData`).
#[must_use]
pub unsafe fn entry_of(name: &str, data: *const sys::ConVarData) -> Value {
    // Raw reads through `addr_of!`: no Rust reference is ever formed over the engine's record,
    // and the type is read as its `i16` repr (a value outside the enum is not UB that way).
    // SAFETY: the caller's contract.
    let (var_type, flags, default, values) = unsafe {
        (
            addr_of!((*data).m_eVarType).cast::<i16>().read_unaligned(),
            addr_of!((*data).m_nFlags).read_unaligned(),
            addr_of!((*data).m_defaultValue).read_unaligned(),
            addr_of!((*data).m_Values).cast::<sys::CVValue_t>(),
        )
    };
    let mut entry = Map::new();
    entry.insert("name".into(), json!(name));
    entry.insert("found".into(), json!(true));
    entry.insert("type".into(), json!(type_name(var_type)));
    entry.insert("flags".into(), json!(flag_names(flags)));
    entry.insert("flagsRaw".into(), json!(flags));
    if flags & sys::FCVAR_PROTECTED != 0 {
        entry.insert("value".into(), Value::Null);
        entry.insert("default".into(), Value::Null);
        entry.insert("redacted".into(), json!(true));
        return Value::Object(entry);
    }
    // SAFETY: slot 0 of `m_Values` is the inline live value; `default` is the record's own
    // pointer (or null, which `read_value_bytes` refuses).
    let (value, default) = unsafe { (read_value_bytes(values), read_value_bytes(default)) };
    match value.and_then(|bytes| value_text(var_type, &bytes)) {
        Some((text, cut)) => {
            entry.insert("value".into(), json!(text));
            if cut {
                entry.insert("valueTruncated".into(), json!(true));
            }
        }
        None => {
            entry.insert("value".into(), Value::Null);
        }
    }
    let default = default
        .and_then(|bytes| value_text(var_type, &bytes))
        .map_or(Value::Null, |(text, _)| json!(text));
    entry.insert("default".into(), default);
    Value::Object(entry)
}

/// The whole reply object for `names` against `source`.
#[must_use]
pub fn build_reply(names: &[String], source: &dyn CvarSource) -> Value {
    let cvars: Vec<Value> = names
        .iter()
        .map(|name| match source.lookup(name) {
            Lookup::NotFound => json!({"name": name, "found": false}),
            Lookup::NoData => json!({
                "name": name, "found": false,
                "error": "ICvar::GetConVarData answered null for a registered cvar",
            }),
            // SAFETY: `Found` carries the engine's live record (the source's contract).
            Lookup::Found(data) => unsafe { entry_of(name, data) },
        })
        .collect();
    json!({ "cvars": cvars })
}

/// The pure body: validate, then look up. Host tests drive it with a synthetic source.
pub fn op_cvar_get_with(params: Option<&str>, source: &dyn CvarSource) -> Result<Value, OpErr> {
    let names = parse_names(params).map_err(|reason| OpErr::new(-32602, "InvalidParams", &reason))?;
    Ok(build_reply(&names, source))
}

/// The engine's `ICvar`, identity-checked against the pinned `CCvar`.
struct LiveCvar {
    iface: *mut sys::ICvar,
}

impl LiveCvar {
    fn acquire() -> Result<LiveCvar, OpErr> {
        let iface = mod_iface(pins::PIN_TIER0, s2sdk::module::versions::CVAR_INTERFACE_VERSION);
        if !obj_is(iface, pins::PIN_TIER0, pins::T0V_CCVAR_VT.value as u32) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "VEngineCvar007 is not the pinned CCvar",
            ));
        }
        Ok(LiveCvar { iface: iface.cast::<sys::ICvar>() })
    }
}

impl CvarSource for LiveCvar {
    fn lookup(&self, name: &str) -> Lookup {
        let Ok(cname) = std::ffi::CString::new(name) else {
            return Lookup::NotFound;
        };
        // SAFETY: `acquire` proved the object is the pinned CCvar and its FindConVar /
        // GetConVarData slots are the pinned targets, so the typed vtable entries are the
        // functions the RE measured.
        unsafe {
            let vtable = &*(*self.iface).vtable;
            let (Some(find), Some(get_data)) = (vtable.FindConVar, vtable.GetConVarData) else {
                return Lookup::NoData;
            };
            // `allow_defensive = false`: a devonly cvar is hidden like in the in-game console.
            #[cfg(windows)]
            let reference = {
                let mut out = core::mem::MaybeUninit::<sys::ConVarRef>::zeroed();
                // MSVC returns the 8-byte `ConVarRef` through a hidden pointer after `this`.
                find(self.iface, out.as_mut_ptr(), cname.as_ptr(), false);
                out.assume_init()
            };
            #[cfg(not(windows))]
            let reference = find(self.iface, cname.as_ptr(), false);
            if reference.m_ConVarAccessIndex == INVALID_ACCESS_INDEX {
                return Lookup::NotFound;
            }
            let data = get_data(self.iface, reference);
            if data.is_null() {
                Lookup::NoData
            } else {
                Lookup::Found(data)
            }
        }
    }
}

/// `cvar.get`'s body against the live engine.
fn op_cvar_get(params: Option<&str>) -> Result<Value, OpErr> {
    // validate before touching the engine: a bad request never needs it
    parse_names(params).map_err(|reason| OpErr::new(-32602, "InvalidParams", &reason))?;
    let live = LiveCvar::acquire()?;
    op_cvar_get_with(params, &live)
}

fn cvar_get(r: &Request) -> String {
    match op_cvar_get(r.params.as_deref()) {
        Ok(result) => control::result_ok(Some(&r.id), &result.to_string()),
        Err(e) if e.data.is_empty() => control::error(Some(&r.id), e.code, e.name),
        Err(e) => control::error_ext(Some(&r.id), e.code, e.name, &e.data),
    }
}

#[cfg(test)]
mod tests;
