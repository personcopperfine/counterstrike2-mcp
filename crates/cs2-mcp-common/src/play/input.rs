//! usercmd-level input through the bridge hook (`input.set/clear/status/uninstall`): virtual
//! buttons via engine2 `CInputValue::ApplyButtonDelta`, view angles via CCSGOInput's override slot.
//! Names are the game's input values (`forward`, `jump`, `attack`, ...). No keypresses, no console.
//!
//! Shared by client-mcp and hammer-mcp: argument validation for `input.set` (nothing is sent
//! before it passes), the wire-safe angle rules and the `input_set` schema.
use crate::tools::ToolError;
use serde::Serialize;
use serde_json::{json, Map, Value};

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InputSet {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub press: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub release: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tap: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u32>,
    /// `Some(None)` clears the override; `Some(Some(a))` sets it; `None` leaves it alone
    #[serde(skip_serializing_if = "Option::is_none")]
    pub angles: Option<Option<[f64; 3]>>,
}


/// Client-side cap for `input_set.frames` (the bridge itself clamps `tap` holds to 1000).
pub const MAX_FRAMES: u32 = 640;
/// Pitch limit for absolute and relative view angles (degrees).
pub const PITCH_LIMIT: f64 = 89.0;
/// Roll limit for view angles (degrees).
pub const ROLL_LIMIT: f64 = 180.0;
/// The bridge copies button names into a 48-byte buffer and truncates longer ones silently
/// (`mcp_bridge.c` `in_each_name`), so longer names are rejected here.
pub const MAX_BUTTON_NAME: usize = 47;

const INPUT_SET_KEYS: [&str; 6] = ["press", "release", "tap", "frames", "angles", "turn"];
const TURN_KEYS: [&str; 2] = ["dyaw", "dpitch"];

/// Relative view turn, resolved against the current override angles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Turn {
    pub dyaw: f64,
    pub dpitch: f64,
}

/// A validated `input_set` request: the wire params plus an optional relative turn.
#[derive(Debug, Clone, PartialEq)]
pub struct InputRequest {
    pub set: InputSet,
    pub turn: Option<Turn>,
}

fn object(args: &Value) -> Result<Map<String, Value>, ToolError> {
    match args {
        Value::Object(m) => Ok(m.clone()),
        Value::Null => Ok(Map::new()),
        _ => Err(ToolError::invalid_arguments("arguments must be an object")),
    }
}

fn reject_unknown(obj: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), ToolError> {
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        None => Ok(()),
        Some(k) => Err(ToolError::invalid_arguments(format!(
            "unknown key {k:?} in {what} (allowed: {})",
            allowed.join(", ")
        ))),
    }
}

fn button_name(v: &Value, key: &str) -> Result<String, ToolError> {
    let s = v.as_str().ok_or_else(|| {
        ToolError::invalid_arguments(format!("{key} must be an array of button names"))
    })?;
    if s.is_empty()
        || s.len() > MAX_BUTTON_NAME
        || s.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\')
    {
        return Err(ToolError::invalid_arguments(format!(
            "button name {s:?} is invalid: 1..={MAX_BUTTON_NAME} bytes, no whitespace, quotes or backslashes"
        )));
    }
    Ok(s.to_string())
}

fn names(obj: &Map<String, Value>, key: &str) -> Result<Vec<String>, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(|v| button_name(v, key)).collect(),
        Some(_) => Err(ToolError::invalid_arguments(format!(
            "{key} must be an array of button names"
        ))),
    }
}

fn frames(obj: &Map<String, Value>) -> Result<Option<u32>, ToolError> {
    match obj.get("frames") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=u64::from(MAX_FRAMES)).contains(n))
            .map(|n| Some(n as u32))
            .ok_or_else(|| {
                ToolError::invalid_arguments(format!(
                    "frames must be an integer in 1..={MAX_FRAMES}"
                ))
            }),
    }
}

/// `[x, y, z]` of finite numbers.
fn triple(v: &Value) -> Option<[f64; 3]> {
    let a = v.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let mut out = [0.0; 3];
    for (slot, n) in out.iter_mut().zip(a) {
        *slot = n.as_f64().filter(|f| f.is_finite())?;
    }
    Some(out)
}

fn round3(x: f64) -> f64 {
    let r = (x * 1000.0).round() / 1000.0;
    if r == 0.0 {
        0.0 // also folds -0.0 to 0.0
    } else {
        r
    }
}

/// Wire-safe view angles `[pitch, yaw, roll]`.
///
/// The bridge's `json_float3` has no exponent parser, and serde prints tiny or huge f64 values in
/// exponent notation (`1e-7`), which the bridge rejects. So every component is rounded to 1e-3
/// (serde then prints plain decimal), yaw wraps into [-180, 180), pitch must be within
/// [-89, 89] and roll within [-180, 180]. `what` names the source in error messages.
pub fn wire_angles(a: [f64; 3], what: &str) -> Result<[f64; 3], ToolError> {
    if !a.iter().all(|f| f.is_finite()) {
        return Err(ToolError::invalid_arguments(format!(
            "{what} angles must be finite"
        )));
    }
    let mut yaw = round3((a[1] + 180.0).rem_euclid(360.0) - 180.0);
    if yaw >= 180.0 {
        yaw -= 360.0; // rounding can land exactly on +180
    }
    let pitch = round3(a[0]);
    if !(-PITCH_LIMIT..=PITCH_LIMIT).contains(&pitch) {
        return Err(ToolError::invalid_arguments(format!(
            "{what} pitch {pitch} is outside [-{PITCH_LIMIT}, {PITCH_LIMIT}]"
        )));
    }
    let roll = round3(a[2]);
    if roll.abs() > ROLL_LIMIT {
        return Err(ToolError::invalid_arguments(format!(
            "{what} roll {roll} is outside [-{ROLL_LIMIT}, {ROLL_LIMIT}]"
        )));
    }
    Ok([pitch, yaw, roll])
}

fn absolute_angles(v: &Value) -> Result<[f64; 3], ToolError> {
    let a = triple(v).ok_or_else(|| {
        ToolError::invalid_arguments(
            "angles must be [pitch, yaw, roll] (three finite numbers) or null",
        )
    })?;
    wire_angles(a, "angles")
}

fn turn(obj: &Map<String, Value>) -> Result<Option<Turn>, ToolError> {
    let v = match obj.get("turn") {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let t = match v {
        Value::Object(t) => t,
        _ => {
            return Err(ToolError::invalid_arguments(
                "turn must be an object {dyaw?, dpitch?}",
            ))
        }
    };
    reject_unknown(t, &TURN_KEYS, "turn")?;
    let read = |key: &str| -> Result<f64, ToolError> {
        match t.get(key) {
            None | Some(Value::Null) => Ok(0.0),
            Some(n) => n.as_f64().filter(|f| f.is_finite()).ok_or_else(|| {
                ToolError::invalid_arguments(format!("turn.{key} must be a number"))
            }),
        }
    };
    Ok(Some(Turn {
        dyaw: read("dyaw")?,
        dpitch: read("dpitch")?,
    }))
}

/// Parse and validate `input_set` arguments. Nothing is sent. Unknown keys are rejected, `frames`
/// needs `tap`, `angles` (absolute) and `turn` (relative) are mutually exclusive, `angles: null`
/// clears the override, and a call that would do nothing is rejected.
pub fn parse_input_set(args: &Value) -> Result<InputRequest, ToolError> {
    let obj = object(args)?;
    reject_unknown(&obj, &INPUT_SET_KEYS, "input_set")?;
    let press = names(&obj, "press")?;
    let release = names(&obj, "release")?;
    let tap = names(&obj, "tap")?;
    let frames = frames(&obj)?;
    if frames.is_some() && tap.is_empty() {
        return Err(ToolError::invalid_arguments(
            "frames applies to tap; give tap as well",
        ));
    }
    let turn = turn(&obj)?;
    let angles =
        match obj.get("angles") {
            None => None,
            Some(_) if turn.is_some() => return Err(ToolError::invalid_arguments(
                "angles and turn cannot be combined: pass an absolute angles or a relative turn",
            )),
            Some(Value::Null) => Some(None),
            Some(v) => Some(Some(absolute_angles(v)?)),
        };
    if press.is_empty()
        && release.is_empty()
        && tap.is_empty()
        && angles.is_none()
        && turn.is_none()
    {
        return Err(ToolError::invalid_arguments(
            "nothing to do: give press, release, tap, angles or turn",
        ));
    }
    Ok(InputRequest {
        set: InputSet {
            press,
            release,
            tap,
            frames,
            angles,
        },
        turn,
    })
}

/// Read the current override angles from an `input.status` result (`angles`: `[p,y,r]` or `null`).
pub fn override_angles(status: &Value) -> Result<[f64; 3], ToolError> {
    match status.get("angles") {
        Some(v) if !v.is_null() => triple(v)
            .ok_or_else(|| ToolError::internal("input.status.angles is not [pitch, yaw, roll]")),
        _ => Err(ToolError::invalid_arguments(
            "turn needs an absolute angle source: no view-angle override is active (input.status.angles is null). Send absolute angles first, or use angles instead of turn",
        )),
    }
}

/// Apply a relative turn to the base override angles, then make the result wire-safe.
pub fn apply_turn(base: [f64; 3], turn: Turn) -> Result<[f64; 3], ToolError> {
    wire_angles(
        [base[0] + turn.dpitch, base[1] + turn.dyaw, base[2]],
        "turn resulting",
    )
}

pub fn input_set_schema() -> Value {
    let button_list = |desc: &str| {
        json!({
            "type": "array",
            "items": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_BUTTON_NAME,
                "pattern": "^[^\\s\"\\\\]+$"
            },
            "description": desc
        })
    };
    json!({
        "type": "object",
        "properties": {
            "press": button_list("buttons held until released (forward, back, jump, duck, attack, use, reload, ...)"),
            "release": button_list("buttons to release"),
            "tap": button_list("buttons held for `frames` input frames"),
            "frames": {"type": "integer", "minimum": 1, "maximum": MAX_FRAMES, "description": "hold length for tap, with tap only", "default": 1},
            "angles": {
                "description": "[pitch, yaw, roll] override; null clears it, absent leaves it",
                "oneOf": [
                    {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                    {"type": "null"}
                ]
            },
            "turn": {
                "type": "object",
                "properties": {
                    "dyaw": {"type": "number", "description": "degrees"},
                    "dpitch": {"type": "number", "description": "degrees"}
                },
                "additionalProperties": false,
                "description": "relative to the active override; not with angles"
            }
        },
        "additionalProperties": false
    })
}

/// The bridge's `input.clear` reply is the status read right after the clear is queued, and its
/// `held` is the state the last frame applied, so it can still list the buttons just cleared (the
/// release lands on the next frame; a protocol-level fix would be a bridge change). Make the tool's
/// reply truthful: `held` is empty, and what was listed moves to `releasedHeld`.
pub fn settle_cleared_reply(reply: &mut serde_json::Value) {
    let Some(object) = reply.as_object_mut() else {
        return;
    };
    let was = object.insert("held".into(), serde_json::json!([]));
    if let Some(was) = was.filter(|w| w.as_array().is_some_and(|a| !a.is_empty())) {
        object.insert("releasedHeld".into(), was);
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn turn_wraps_yaw_and_checks_pitch() {
        let a = apply_turn(
            [0.0, 170.0, 0.0],
            Turn {
                dyaw: 30.0,
                dpitch: 0.0,
            },
        )
        .unwrap();
        assert_eq!(a, [0.0, -160.0, 0.0]);
        let a = apply_turn(
            [0.0, -170.0, 0.0],
            Turn {
                dyaw: -30.0,
                dpitch: 0.0,
            },
        )
        .unwrap();
        assert_eq!(a, [0.0, 160.0, 0.0]);
        assert!(apply_turn(
            [80.0, 0.0, 0.0],
            Turn {
                dyaw: 0.0,
                dpitch: 10.0
            }
        )
        .is_err());
    }

    /// Serialized `angles` array of an InputSet (the keys themselves contain 'e', so only the
    /// array is inspected).
    fn wire_form(a: [f64; 3]) -> String {
        let set = InputSet {
            angles: Some(Some(a)),
            ..Default::default()
        };
        let s = serde_json::to_string(&set).unwrap();
        s[s.find('[').unwrap()..].to_string()
    }

    #[test]
    fn angles_never_serialize_with_exponent() {
        // (input, expected wire value): tiny, negative-tiny, wrap-edge and limit-edge components
        let cases = [
            ([1e-7, 359.9999999, -1e-9], [0.0, 0.0, 0.0]),
            ([-1e-9, 359.9999999, 1e-7], [0.0, 0.0, 0.0]),
            ([0.0004, 179.9996, -179.9996], [0.0, -180.0, -180.0]),
            ([1e-5, -2.5e-4, 1e-3], [0.0, 0.0, 0.001]),
        ];
        for (input, expected) in cases {
            let wired = wire_angles(input, "test").unwrap();
            assert_eq!(wired, expected, "{input:?}");
            let text = wire_form(wired);
            assert!(!text.contains('e') && !text.contains('E'), "{text}");
        }
    }

    #[test]
    fn exponent_inputs_round_to_plain_decimal() {
        assert_eq!(
            wire_angles([1e-7, 359.9999999, -1e-9], "t").unwrap(),
            [0.0, 0.0, 0.0]
        );
        assert_eq!(
            wire_angles([-2.5e-4, 1e-5, 0.0], "t").unwrap(),
            [0.0, 0.0, 0.0]
        );
        assert_eq!(
            wire_angles([10.0, 359.9999999, 0.0], "t").unwrap(),
            [10.0, 0.0, 0.0]
        );
        assert_eq!(
            wire_angles([10.0, 180.0, 0.0], "t").unwrap(),
            [10.0, -180.0, 0.0]
        );
    }

    #[test]
    fn roll_out_of_range_is_rejected() {
        assert!(wire_angles([0.0, 0.0, 180.0], "t").is_ok());
        assert!(wire_angles([0.0, 0.0, 180.5], "t").is_err());
        assert!(wire_angles([0.0, 0.0, -181.0], "t").is_err());
    }

    #[test]
    fn turn_result_is_wire_safe() {
        let a = apply_turn(
            [0.0, 179.9996, 0.0],
            Turn {
                dyaw: 0.0004,
                dpitch: 1e-7,
            },
        )
        .unwrap();
        let text = wire_form(a);
        assert!(!text.contains('e') && !text.contains('E'), "{text}");
        assert_eq!(a, [0.0, -180.0, 0.0]);
    }

    #[test]
    fn a_cleared_reply_lists_nothing_held() {
        let mut r = serde_json::json!({"installed": true, "held": ["duck"]});
        settle_cleared_reply(&mut r);
        assert_eq!(r["held"], serde_json::json!([]));
        assert_eq!(r["releasedHeld"], serde_json::json!(["duck"]));
        let mut r = serde_json::json!({"installed": true, "held": []});
        settle_cleared_reply(&mut r);
        assert!(r.get("releasedHeld").is_none());
    }
}
