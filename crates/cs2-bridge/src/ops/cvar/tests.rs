//! Host tests of `cvar.get`: the parser, the value decoder, the entry builder over synthetic
//! `ConVarData` records, and the reply shape. The engine lookups (`LiveCvar`) are the live half.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;

fn record(var_type: sys::EConVarType, flags: u64, live: [u8; 16], default: *mut sys::CVValue_t) -> sys::ConVarData {
    // SAFETY: an all-zero `ConVarData` is a valid value (raw pointers, integers, byte arrays).
    let mut data: sys::ConVarData = unsafe { core::mem::zeroed() };
    data.m_eVarType = var_type;
    data.m_nFlags = flags;
    data.m_Values = live;
    data.m_defaultValue = default;
    data
}

fn int32(v: i32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..4].copy_from_slice(&v.to_le_bytes());
    bytes
}

fn value_of(bytes: [u8; 16]) -> sys::CVValue_t {
    // SAFETY: `CVValue_t` is 16 plain bytes (asserted in the module); any pattern is valid.
    unsafe { core::mem::transmute::<[u8; 16], sys::CVValue_t>(bytes) }
}

#[test]
fn names_are_validated() {
    let ok = |p: &str| parse_names(Some(p));
    assert_eq!(ok(r#"{"names":["mp_c4timer","sv_cheats"]}"#).unwrap(), vec!["mp_c4timer", "sv_cheats"]);
    let sixty_four: Vec<String> = (0..64).map(|i| format!("c{i}")).collect();
    assert_eq!(ok(&json!({"names": sixty_four}).to_string()).unwrap().len(), 64);
    let sixty_five: Vec<String> = (0..65).map(|i| format!("c{i}")).collect();
    for bad in [
        r#"{}"#.to_string(),
        r#"{"names":[]}"#.to_string(),
        json!({"names": sixty_five}).to_string(),
        r#"{"names":[1]}"#.to_string(),
        r#"{"names":[""]}"#.to_string(),
        r#"{"names":["a\nb"]}"#.to_string(),
        json!({"names": ["x".repeat(129)]}).to_string(),
        r#"{"names":"mp_c4timer"}"#.to_string(),
        "not json".to_string(),
    ] {
        assert!(parse_names(Some(&bad)).is_err(), "{bad}");
    }
    assert!(parse_names(None).is_err());
}

#[test]
fn flag_and_type_names_come_from_the_generated_consts() {
    assert_eq!(flag_names(sys::FCVAR_REPLICATED | sys::FCVAR_CHEAT), vec!["replicated", "cheat"]);
    assert_eq!(flag_names(0), Vec::<&str>::new());
    assert_eq!(flag_names(1 << 40), Vec::<&str>::new(), "unnamed bits stay in flagsRaw");
    assert_eq!(type_name(3), "int32");
    assert_eq!(type_name(9), "string");
    assert_eq!(type_name(-1), "unknown");
    assert_eq!(type_name(99), "unknown");
}

#[test]
fn values_print_like_the_engines_value_to_string() {
    let text = |t: sys::EConVarType, bytes: [u8; 16]| value_text(t.0, &bytes).map(|(s, _)| s);
    assert_eq!(text(sys::EConVarType::EConVarType_Bool, [1; 16]).unwrap(), "true");
    assert_eq!(text(sys::EConVarType::EConVarType_Bool, [0; 16]).unwrap(), "false");
    assert_eq!(text(sys::EConVarType::EConVarType_Int32, int32(-7)).unwrap(), "-7");
    assert_eq!(text(sys::EConVarType::EConVarType_UInt32, int32(-1)).unwrap(), "4294967295");
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(-5i64).to_le_bytes());
    assert_eq!(text(sys::EConVarType::EConVarType_Int64, b).unwrap(), "-5");
    b[..4].copy_from_slice(&1.5f32.to_le_bytes());
    assert_eq!(text(sys::EConVarType::EConVarType_Float32, b).unwrap(), "1.5");
    b[..8].copy_from_slice(&0.25f64.to_le_bytes());
    assert_eq!(text(sys::EConVarType::EConVarType_Float64, b).unwrap(), "0.25");
    let mut color = [0u8; 16];
    color[..4].copy_from_slice(&[10, 20, 30, 255]);
    assert_eq!(text(sys::EConVarType::EConVarType_Color, color).unwrap(), "10 20 30");
    color[3] = 128;
    assert_eq!(text(sys::EConVarType::EConVarType_Color, color).unwrap(), "10 20 30 128");
    let mut v = [0u8; 16];
    for (i, x) in [1.0f32, 2.5, -3.0, 4.0].iter().enumerate() {
        v[i * 4..i * 4 + 4].copy_from_slice(&x.to_le_bytes());
    }
    assert_eq!(text(sys::EConVarType::EConVarType_Vector2, v).unwrap(), "1 2.5");
    assert_eq!(text(sys::EConVarType::EConVarType_Vector3, v).unwrap(), "1 2.5 -3");
    assert_eq!(text(sys::EConVarType::EConVarType_Vector4, v).unwrap(), "1 2.5 -3 4");
    assert!(value_text(-1, &v).is_none(), "an unknown type has no text");
    // a null string pointer is the empty string; a real one is followed
    assert_eq!(text(sys::EConVarType::EConVarType_String, [0; 16]).unwrap(), "");
    let owned = CString::new("de_dust2").unwrap();
    let mut s = [0u8; 16];
    s[..8].copy_from_slice(&(owned.as_ptr() as usize as u64).to_le_bytes());
    assert_eq!(text(sys::EConVarType::EConVarType_String, s).unwrap(), "de_dust2");
    let long = CString::new("x".repeat(VALUE_MAX_BYTES + 10)).unwrap();
    s[..8].copy_from_slice(&(long.as_ptr() as usize as u64).to_le_bytes());
    let (shown, cut) = value_text(sys::EConVarType::EConVarType_String.0, &s).unwrap();
    assert_eq!((shown.len(), cut), (VALUE_MAX_BYTES, true));
}

#[test]
fn an_entry_reads_type_flags_value_and_default() {
    let mut default = value_of(int32(40));
    let data = record(
        sys::EConVarType::EConVarType_Int32,
        sys::FCVAR_REPLICATED | sys::FCVAR_NOTIFY,
        int32(45),
        &mut default,
    );
    // SAFETY: `data` is a live, fully initialised record.
    let entry = unsafe { entry_of("mp_c4timer", &data) };
    assert_eq!(
        entry,
        json!({
            "name": "mp_c4timer", "found": true, "type": "int32",
            "flags": ["notify", "replicated"], "flagsRaw": sys::FCVAR_REPLICATED | sys::FCVAR_NOTIFY,
            "value": "45", "default": "40",
        })
    );
}

#[test]
fn a_missing_default_pointer_is_null_and_a_protected_cvar_is_redacted() {
    let data = record(sys::EConVarType::EConVarType_Int32, 0, int32(1), core::ptr::null_mut());
    // SAFETY: a live, initialised record.
    let entry = unsafe { entry_of("a", &data) };
    assert_eq!(entry["value"], json!("1"));
    assert_eq!(entry["default"], Value::Null);

    let owned = CString::new("hunter2").unwrap();
    let mut live = [0u8; 16];
    live[..8].copy_from_slice(&(owned.as_ptr() as usize as u64).to_le_bytes());
    let mut default = value_of(live);
    let secret = record(sys::EConVarType::EConVarType_String, sys::FCVAR_PROTECTED, live, &mut default);
    // SAFETY: a live, initialised record.
    let entry = unsafe { entry_of("sv_password", &secret) };
    assert_eq!(entry["value"], Value::Null);
    assert_eq!(entry["default"], Value::Null);
    assert_eq!(entry["redacted"], json!(true));
    assert!(!entry.to_string().contains("hunter2"));
}

struct Fake {
    records: HashMap<&'static str, Lookup>,
    asked: RefCell<Vec<String>>,
}

impl CvarSource for Fake {
    fn lookup(&self, name: &str) -> Lookup {
        self.asked.borrow_mut().push(name.to_string());
        self.records.get(name).copied().unwrap_or(Lookup::NotFound)
    }
}

#[test]
fn the_reply_has_one_entry_per_name_in_order() {
    let mut default = value_of(int32(1));
    let data = record(sys::EConVarType::EConVarType_Bool, sys::FCVAR_CHEAT, [1; 16], &mut default);
    let source = Fake {
        records: HashMap::from([
            ("sv_cheats", Lookup::Found(&data)),
            ("broken", Lookup::NoData),
        ]),
        asked: RefCell::new(Vec::new()),
    };
    let reply = op_cvar_get_with(
        Some(r#"{"names":["nope","sv_cheats","broken"]}"#),
        &source,
    )
    .unwrap();
    let cvars = reply["cvars"].as_array().unwrap();
    assert_eq!(cvars.len(), 3);
    assert_eq!(cvars[0], json!({"name": "nope", "found": false}));
    assert_eq!(cvars[1]["name"], json!("sv_cheats"));
    assert_eq!(cvars[1]["value"], json!("true"));
    assert_eq!(cvars[1]["flags"], json!(["cheat"]));
    assert_eq!(cvars[2]["found"], json!(false));
    assert!(cvars[2]["error"].as_str().unwrap().contains("GetConVarData"));
    assert_eq!(*source.asked.borrow(), vec!["nope", "sv_cheats", "broken"]);
}

#[test]
fn invalid_params_ask_the_engine_nothing_and_answer_invalid_params() {
    let source = Fake { records: HashMap::new(), asked: RefCell::new(Vec::new()) };
    for bad in [r#"{"names":[]}"#, r#"{"names":[3]}"#, r#"{}"#] {
        let err = op_cvar_get_with(Some(bad), &source).unwrap_err();
        assert_eq!((err.code, err.name), (-32602, "InvalidParams"), "{bad}");
    }
    assert!(source.asked.borrow().is_empty());
}

#[test]
fn the_wire_reply_is_the_result_object_in_the_envelope() {
    let source = Fake { records: HashMap::new(), asked: RefCell::new(Vec::new()) };
    let result = op_cvar_get_with(Some(r#"{"names":["zzz"]}"#), &source).unwrap();
    let line = control::result_ok(Some("7"), &result.to_string());
    assert!(line.contains(r#""result":{"cvars":[{"found":false,"name":"zzz"}]}"#), "{line}");
    assert!(line.ends_with("}\n"));
}

#[test]
fn the_called_slots_are_the_generated_ones() {
    // the pins the object check holds (generated from the sigs rows) name the same slots the
    // generated vtable's consts do
    let slots: Vec<(usize, &str)> = pins::PIN_SLOTS
        .iter()
        .filter(|s| s.vt_rva == pins::T0V_CCVAR_VT.value as u32)
        .map(|s| (s.slot as usize, s.what))
        .collect();
    assert!(slots.contains(&(sys::ICvar_slots::FindConVar, "ICvar::FindConVar")), "{slots:?}");
    assert!(slots.contains(&(sys::ICvar_slots::GetConVarData, "ICvar::GetConVarData")), "{slots:?}");
}
