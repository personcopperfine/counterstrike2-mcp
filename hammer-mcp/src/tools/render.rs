//! `render_view` / `render_world` (WP-D): the POC's camera plumbing over `view.render` /
//! `world.render` (mcp_bridge.c `op_view_render` / `op_world_render`; live-verified
//! `test_view_render.py` for `view.render` — the orchestrator re-runs the live halves).
//!
//! The two tools share [`camera_shape`] (the POC `h_render_view`/`h_render_world` camera code)
//! and the file/inline pipeline ([`super::particle_render`] — the same one `model_render` uses),
//! and differ only where the bridge methods differ:
//!
//! * `render_view` renders the **LIVE document** (unsaved edits included) through Hammer's own
//!   offscreen renderer (`RenderViewToMemory`, the map-thumbnail path) with
//!   [`super::particle_render::IMAGE_MIME`] attached inline by default; no camera = the user's
//!   current 3D viewport camera. It runs on the active document, so a bridge without one answers
//!   `-32003 NoActiveDocument` (dispatch) or `-32019 NoViewport` (`op_view_render`'s chain).
//! * `render_world` renders the map **running in the engine** (compiled .vpk lighting, no
//!   Hammer) from a required camera — origin+angles, origin+lookAt or lookAt(-only orbit) —
//!   sizes `16..=8192` px per side and at most 33554432 pixels, inlined by default only up to
//!   [`INLINE_MAX_PIXELS`] (1920x1080). No map loaded in the engine is *not* an error from the
//!   engine: after `disconnect` (or with none loaded) `GetGameSceneWorld` still returns the
//!   empty scene world, so `render_world {lookAt}` answers a uniform-black image plus the blank
//!   note (review-2 L6 live); only an engine with no scene world *object* at all answers
//!   `-32021 NoGameWorld`, which the tool maps like any other bridge error.
//!
//! Size bounds are mirrored off `particle_render`'s own rules (nothing
//! out of bounds reaches the wire; projection values are validated against the bridge's
//! documented ranges too — a `zfar` alone is checked against `(0, 1e6]`, and `render_world`'s
//! against its own default `znear` of 4; a *view*'s effective `znear` is the viewport's and only
//! the bridge can refuse that pair). The notes ([`render_view_notes`], the POC's) report a
//! blank image (`nonBlackPixels == 0` or `maxLuma - minLuma < 4`) and the live/compiled caveat
//! when this server has unsaved edits ([`crate::state::HammerState::unsaved_edits`]).
//!
//! Naming follows `particle_render`/`model_render` (review-2 L3): a caller-given `name` is
//! validated locally and sent; without one the tool sends **no** `name` and the *bridge* names
//! the file (`render_<GetTickCount64>_<bridge seq>`, its `render_name` default — its render
//! sequence is global, so two hammer-mcp processes can no longer collide on a same-second
//! client-side stem and overwrite each other's PNG).
//!
//! When the render succeeded but the Windows path does not map to a host file, the missed
//! inline is what changes the shape (review-2 L4): with `inline:true` the tool fails hard
//! (`PathNotMapped`/`RenderFileMissing`, nothing retryable — the bridge already wrote the PNG
//! and only the path map is wrong); with `inline:false` it answers the bridge's own result with
//! `hostPath: null` and a note, and skips the local pixel decode (`localStats`) — the bridge's
//! `stats` are already in the reply, and re-rendering a 8192x4096 world render to chase the
//! same path error is exactly what the old hard error invited.
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::capture::INLINE_MAX_PIXELS;
use cs2_mcp_common::tools::{
    CallCtx, Image, Mutability::ReadOnly, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};

use super::particle_render::{IMAGE_MIME, render_name, render_pixels};

/// The two tools this module registers, at their POC_ORDER positions
/// (`tools/hammer.rs::POC_TOOL_ORDER`, between `get_entity` and `list_meshes`).
pub const RENDER_TOOL_ORDER: [&str; 2] = ["render_view", "render_world"];

pub fn register_one(r: &mut Registry<HammerState>, name: &'static str) {
    match name {
        "render_view" => r.register(
            ToolSpec::new(
                "render_view",
                "Render the LIVE map (unsaved edits included) from a camera with Hammer's offscreen renderer: PNG under <app>/renders/ plus an inline image (downscaled above 1920x1080; inline:false skips). \
                 Shows editor helpers/gizmos/grid (hideToolMaterials hides trigger/clip/tools volumes). Not for: the compiled game world (render_world). \
                 Camera: none = current 3D viewport; origin+angles; origin+lookAt; or lookAt alone to orbit (distance/pitch/yaw). angles [pitch,yaw,roll] degrees, pitch>0 looks down, yaw 0 = +X, 90 = +Y; fov is HORIZONTAL degrees.",
                ReadOnly,
                &["view.render"],
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "origin": vec3p(""),
                    "angles": vec3p(""),
                    "lookAt": vec3p(""),
                    "distance": {"type": "number", "exclusiveMinimum": 0, "default": 256},
                    "pitch": {"type": "number", "minimum": -89, "maximum": 89, "default": 30},
                    "yaw": {"type": "number", "default": 45},
                    "fov": {"type": "number", "exclusiveMinimum": 1, "exclusiveMaximum": 179,
                            "description": "default: the viewport's"},
                    "width": {"type": "integer", "minimum": 16, "maximum": 4096, "default": 640},
                    "height": {"type": "integer", "minimum": 16, "maximum": 4096, "default": 360},
                    "znear": {"type": "number", "exclusiveMinimum": 0},
                    "zfar": {"type": "number", "exclusiveMinimum": 0},
                    "hideToolMaterials": {"type": "boolean", "default": false},
                    "name": {"type": "string",
                             "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$",
                             "description": "file stem under renders/"},
                    "inline": {"type": "boolean", "default": true},
                },
                "additionalProperties": false
            })),
            render_view,
        ),
        "render_world" => r.register(
            ToolSpec::new(
                "render_world",
                "Render the map loaded in the ENGINE (compiled .vpk: game lighting, sky, entities, no editor helpers) from a required camera, without Hammer. Not for: the live editor view (render_view). \
                 Camera: origin+angles, origin+lookAt, or lookAt alone to orbit (distance/pitch/yaw). angles [pitch,yaw,roll] degrees, pitch>0 looks down, yaw 0 = +X; fov is HORIZONTAL degrees. \
                 Up to 8192 px per side and 33554432 pixels, independent of the window; image block inline by default only up to 1920x1080 (inline:true forces, false refuses). \
                 No map loaded renders a black empty scene (blank note).",
                ReadOnly,
                &["world.render"],
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "origin": vec3p("required"),
                    "angles": vec3p("required"),
                    "lookAt": vec3p(""),
                    "distance": {"type": "number", "exclusiveMinimum": 0, "default": 256},
                    "pitch": {"type": "number", "minimum": -89, "maximum": 89, "default": 30},
                    "yaw": {"type": "number", "default": 45},
                    "fov": {"type": "number", "exclusiveMinimum": 1, "exclusiveMaximum": 179,
                            "default": 90},
                    "width": {"type": "integer", "minimum": 16, "maximum": 8192, "default": 1920},
                    "height": {"type": "integer", "minimum": 16, "maximum": 8192, "default": 1080},
                    "znear": {"type": "number", "exclusiveMinimum": 0, "default": 4},
                    "zfar": {"type": "number", "exclusiveMinimum": 0, "default": 30000},
                    "name": {"type": "string",
                             "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$",
                             "description": "file stem under renders/"},
                    "inline": {"type": "boolean",
                               "description": "default: only if width*height <= 1920*1080"},
                },
                "additionalProperties": false
            })),
            render_world,
        ),
        other => panic!("render.rs does not register {other}"),
    }
}

/// The `vec3` JSON-schema fragment both tool schemas share (the POC `vec3`), with a
/// description.
fn vec3p(description: &str) -> Value {
    let mut v = json!({
        "type": "array",
        "items": {"type": "number"},
        "minItems": 3,
        "maxItems": 3,
    });
    if !description.is_empty() {
        v["description"] = json!(description);
    }
    v
}

// ------------------------------------------------------------------ arguments

/// [`camera_shape`]'s result: what a camera was built from and how.
struct Camera {
    origin: Option<[f64; 3]>,
    angles: Option<[f64; 3]>,
    /// the POC's modes: viewport / explicit / mixed / lookAt / orbit (reported as
    /// `camera.mode` next to the bridge's camera values)
    mode: &'static str,
    look_at: Option<[f64; 3]>,
}

/// The POC's `_vec3`: `[x, y, z]`, three finite numbers, no bools.
fn vec3(args: &Map<String, Value>, key: &str) -> Result<Option<[f64; 3]>, ToolError> {
    let Some(v) = args.get(key) else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let bad = || ToolError::invalid_arguments(format!("{key} must be [x, y, z] (3 finite numbers)"));
    let arr = v.as_array().ok_or_else(bad)?;
    if arr.len() != 3 {
        return Err(bad());
    }
    let mut out = [0.0; 3];
    for (i, x) in arr.iter().enumerate() {
        out[i] = x.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
    }
    Ok(Some(out))
}

/// A scalar argument (fov / znear / zfar / orbit params): a finite number, no bools.
fn number(args: &Map<String, Value>, key: &str) -> Result<Option<f64>, ToolError> {
    fn num(v: Option<&Value>, key: &str) -> Result<Option<f64>, ToolError> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite())
                .map(Some)
                .ok_or_else(|| ToolError::invalid_arguments(format!("{key} must be a number"))),
        }
    }
    num(args.get(key), key)
}

/// A required-finite number (the orbit params): `.expect_number` behaviour.
fn number_or(args: &Map<String, Value>, key: &str, dflt: f64) -> Result<f64, ToolError> {
    Ok(number(args, key)?.unwrap_or(dflt))
}

/// The camera plumbing both tools share (POC `h_render_view`/`h_render_world`): angles and
/// lookAt are alternatives; distance/pitch/yaw are only accepted with a lookAt (they place the
/// orbit camera when there is no origin, and are ignored behind an explicit one); lookAt
/// computes angles from the resulting position. Whether a camera is required at all is the
/// caller's rule (`render_world`'s is "needs a camera").
fn camera_shape(args: &Map<String, Value>) -> Result<Camera, ToolError> {
    let origin = vec3(args, "origin")?;
    let angles = vec3(args, "angles")?;
    let look_at = vec3(args, "lookAt")?;
    if angles.is_some() && look_at.is_some() {
        return Err(ToolError::invalid_arguments(
            "give either angles or lookAt, not both",
        ));
    }
    if look_at.is_none()
        && args.keys().any(|k| matches!(k.as_str(), "distance" | "pitch" | "yaw"))
    {
        return Err(ToolError::invalid_arguments(
            "distance/pitch/yaw place an orbit camera and need lookAt",
        ));
    }
    let mut cam = Camera {
        origin,
        angles,
        mode: "viewport",
        look_at,
    };
    if let Some(target) = look_at {
        if let Some(origin) = cam.origin {
            cam.mode = "lookAt";
            cam.angles = Some(look_angles(origin, target)?);
        } else {
            cam.mode = "orbit";
            let distance = number_or(args, "distance", 256.0)?;
            let pitch = number_or(args, "pitch", 30.0)?;
            let yaw = number_or(args, "yaw", 45.0)?;
            if !(distance > 0.0 && (-89.0..=89.0).contains(&pitch) && yaw.is_finite()) {
                return Err(ToolError::invalid_arguments(
                    "orbit: distance > 0, pitch in [-89, 89]",
                ));
            }
            let pos = orbit_origin(target, distance, pitch, yaw);
            cam.angles = Some(look_angles(pos, target)?);
            cam.origin = Some(pos);
        }
    } else if origin.is_some() || angles.is_some() {
        cam.mode = match (origin, angles) {
            (Some(_), Some(_)) => "explicit",
            _ => "mixed",
        };
    }
    Ok(cam)
}

/// Source QAngle `[pitch, yaw, roll]` that aims `origin` at `target` (POC `look_angles`):
/// pitch>0 looks down, yaw 0 = +X, rounded to 4 decimals; a target equal to the origin is
/// refused.
pub(crate) fn look_angles(origin: [f64; 3], target: [f64; 3]) -> Result<[f64; 3], ToolError> {
    let d = [target[0] - origin[0], target[1] - origin[1], target[2] - origin[2]];
    if d.iter().all(|c| c.abs() < 1e-6) {
        return Err(ToolError::invalid_arguments("lookAt equals origin"));
    }
    let yaw = f64::atan2(d[1], d[0]).to_degrees();
    let pitch = -f64::atan2(d[2], f64::hypot(d[0], d[1])).to_degrees();
    Ok([round4(pitch), round4(yaw), 0.0])
}

/// The camera position `distance` units back from `target` along (pitch, yaw)
/// (POC `orbit_origin`; `pitch>0` = from above, yaw 0 = +X), rounded to 3 decimals.
pub(crate) fn orbit_origin(target: [f64; 3], distance: f64, pitch: f64, yaw: f64) -> [f64; 3] {
    let (p, y) = (pitch.to_radians(), yaw.to_radians());
    let fwd = [p.cos() * y.cos(), p.cos() * y.sin(), -p.sin()];
    [
        round3(target[0] - fwd[0] * distance),
        round3(target[1] - fwd[1] * distance),
        round3(target[2] - fwd[2] * distance),
    ]
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn round3(v: f64) -> f64 {
    (v * 1_000.0).round() / 1_000.0
}

/// `width`/`height` (the bridge's bounds: `op_view_render` 16..4096 & 8388608 px,
/// `op_world_render` 16..8192 & 33554432 px); an out-of-range or non-integer value is
/// `InvalidParams` with the bridge's message so the model sees the same shape as a live
/// refusal.
fn size(
    args: &Map<String, Value>,
    key: &str,
    dflt: u64,
    max_side: u64,
    max_pixels: u64,
) -> Result<u64, ToolError> {
    let v = match args.get(key) {
        None | Some(Value::Null) => return Ok(dflt),
        Some(v) => v,
    };
    let n = v
        .as_u64()
        .filter(|n| (16..=max_side).contains(n))
        .ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "{key}: 16..{max_side} each, at most {max_pixels} pixels"
            ))
        })?;
    Ok(n)
}

/// The combined pixel-count gate of the two bounds (`width * height` at most `max_pixels`).
fn check_pixels(w: u64, h: u64, max_pixels: u64, max_side: u64) -> Result<(), ToolError> {
    let bad = || {
        ToolError::invalid_arguments(format!(
            "width/height: 16..{max_side} each, at most {max_pixels} pixels"
        ))
    };
    if w.checked_mul(h).is_some_and(|n| n > max_pixels) {
        return Err(bad());
    }
    Ok(())
}

/// `fov`/`znear`/`zfar`: sent only when given, with the bridge's validation mirrored (`(1, 179)`
/// degrees; `znear > 0`, `zfar > znear`, `zfar <= 1e6`) so an out-of-range value never reaches
/// the wire. `zfar` alone is checked too (review-2 N3): `(0, 1e6]` keeps the claim for view, and
/// `render_world`'s own default `znear` of 4 turns its pair rule local (`op_world_render` would
/// refuse anything lower). A `view`'s effective `znear` is the viewport's — only the bridge can
/// rule on that pair, which the module header says so.
fn projection(args: &Map<String, Value>, world: bool) -> Result<[Option<f64>; 3], ToolError> {
    let fov = number(args, "fov")?;
    if let Some(fov) = fov {
        if !(fov > 1.0 && fov < 179.0) {
            return Err(ToolError::invalid_arguments(
                "fov must be in (1, 179) degrees (horizontal)",
            ));
        }
    }
    let znear = number(args, "znear")?;
    let zfar = number(args, "zfar")?;
    let zfar_default = if world { 4.0 } else { 1.0 };
    if znear.is_some_and(|n| n <= 0.0)
        || zfar.is_some_and(|f| !(f > 0.0 && f <= 1_000_000.0))
        || matches!(zfar, Some(f) if f <= zfar_min_given(znear, zfar_default))
    {
        return Err(ToolError::invalid_arguments("znear/zfar"));
    }
    if let (Some(n), Some(f)) = (znear, zfar) {
        if !(f > n && f <= 1_000_000.0) {
            return Err(ToolError::invalid_arguments("znear/zfar"));
        }
    }
    Ok([fov, znear, zfar])
}

/// The smallest `zfar` a `znear`-less call may carry (review-2 N3): the bridge's own pair rule
/// against the method's default `znear` (world: its 4; view: the bridge's fallback 1).
fn zfar_min_given(znear: Option<f64>, zfar_default: f64) -> f64 {
    znear.unwrap_or(zfar_default)
}

/// One arg set: everything that passed validation, ready to go on the wire.
struct Args {
    width: u64,
    height: u64,
    fov: Option<f64>,
    znear: Option<f64>,
    zfar: Option<f64>,
    hide_tools: bool,
    name: Option<String>,
    inline: bool,
    cam: Camera,
}

/// The argument keys `render_view` accepts (the POC tool schema).
const VIEW_KEYS: &[&str] = &[
    "origin", "angles", "lookAt", "distance", "pitch", "yaw", "fov", "width", "height", "znear",
    "zfar", "hideToolMaterials", "name", "inline",
];
/// The argument keys `render_world` accepts (`hideToolMaterials` is viewport-only).
const WORLD_KEYS: &[&str] = &[
    "origin", "angles", "lookAt", "distance", "pitch", "yaw", "fov", "width", "height", "znear",
    "zfar", "name", "inline",
];

fn unknown_key(obj: &Map<String, Value>, keys: &[&str]) -> Result<(), ToolError> {
    if let Some(bad) = obj.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(ToolError::invalid_arguments(format!(
            "unknown argument {bad:?}"
        )));
    }
    Ok(())
}

/// `render_view`'s parameters (the POC `h_render_view` validation order — camera consistency
/// first, then sizes, then projection, then the name) and its defaults: 640x360, the viewport
/// camera, inline on, `hideToolMaterials` sent only when true.
fn parse_view(args: &Value) -> Result<Args, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    unknown_key(obj, VIEW_KEYS)?;
    let cam = camera_shape(obj)?;
    let [fov, znear, zfar] = projection(obj, false)?;
    let width = size(obj, "width", 640, 4096, 8_388_608)?;
    let height = size(obj, "height", 360, 4096, 8_388_608)?;
    check_pixels(width, height, 8_388_608, 4096)?;
    Ok(Args {
        width,
        height,
        fov,
        znear,
        zfar,
        hide_tools: bool_arg(obj, "hideToolMaterials", false)?,
        name: name_arg(obj)?,
        inline: bool_arg(obj, "inline", true)?,
        cam,
    })
}

/// `render_world`'s parameters (the POC `h_render_world` validation order; a camera is
/// required) and its defaults: 1920x1080, inline only up to 1920x1080 pixels.
fn parse_world(args: &Value) -> Result<Args, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    unknown_key(obj, WORLD_KEYS)?;
    let cam = camera_shape(obj)?;
    if cam.origin.is_none() || cam.angles.is_none() {
        return Err(ToolError::invalid_arguments(
            "render_world needs a camera: origin+angles, origin+lookAt or lookAt",
        ));
    }
    let [fov, znear, zfar] = projection(obj, true)?;
    let width = size(obj, "width", 1920, 8192, 33_554_432)?;
    let height = size(obj, "height", 1080, 8192, 33_554_432)?;
    check_pixels(width, height, 33_554_432, 8192)?;
    // the POC's default: inline only small enough renders (the shared 1920x1080 cap)
    Ok(Args {
        width,
        height,
        fov,
        znear,
        zfar,
        hide_tools: false,
        name: name_arg(obj)?,
        inline: bool_arg(obj, "inline", width * height <= INLINE_MAX_PIXELS)?,
        cam,
    })
}

/// An optional boolean argument with a default (`inline`, `hideToolMaterials`).
fn bool_arg(obj: &Map<String, Value>, key: &str, dflt: bool) -> Result<bool, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(dflt),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments(format!("{key}: boolean expected"))),
    }
}

/// The `name` argument: validated against the bridge's `render_name` rule (bad →
/// `InvalidParams`, nothing sent); `None` = the tool sends **no** `name` and the bridge names
/// the file (review-2 L3: its `render_<GetTickCount64>_<seq>` default, unique per bridge, so
/// concurrent hammer-mcp processes cannot overwrite each other's PNG).
fn name_arg(obj: &Map<String, Value>) -> Result<Option<String>, ToolError> {
    match obj.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let raw = v
                .as_str()
                .ok_or_else(|| ToolError::invalid_arguments("name: a string is required"))?;
            Ok(Some(render_name(raw)?))
        }
    }
}

/// The bridge-method params both tools send (a camera is always fully explicit on the wire;
/// the viewport default is a `render_view` concept the tool resolves into no camera args; a
/// caller-given `name` is sent as-is, and no `name` at all lets the bridge name the file,
/// review-2 L3).
fn wire_params(parsed: &Args, is_view: bool) -> Value {
    let mut params = Map::new();
    params.insert("width".into(), json!(parsed.width));
    params.insert("height".into(), json!(parsed.height));
    if let Some(origin) = parsed.cam.origin {
        params.insert("origin".into(), json!(origin));
    }
    if let Some(angles) = parsed.cam.angles {
        params.insert("angles".into(), json!(angles));
    }
    if let Some(fov) = parsed.fov {
        params.insert("fov".into(), json!(fov));
    }
    if let Some(znear) = parsed.znear {
        params.insert("znear".into(), json!(znear));
    }
    if let Some(zfar) = parsed.zfar {
        params.insert("zfar".into(), json!(zfar));
    }
    if is_view && parsed.hide_tools {
        params.insert("hideToolMaterials".into(), json!(true));
    }
    if let Some(stem) = &parsed.name {
        params.insert("name".into(), json!(stem));
    }
    Value::Object(params)
}

/// The shared pipeline: call the render method, then assemble the POC's result over the bridge's
/// object (camera mode/lookAt, renderMs/readbackMs, notes), the host path and the shared inline
/// policy (`localStats` + `inline` + the image block; review-2 L4 for the unmapped/missing host
/// path).
pub(crate) async fn run(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
    method: &'static str,
    is_view: bool,
) -> Result<ToolOutput, ToolError> {
    let parsed = if is_view {
        parse_view(&args)?
    } else {
        parse_world(&args)?
    };
    let resp = {
        // lock first: the deadline is computed from the budget the moment the request goes out
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        bridge
            .request(
                method,
                wire_params(&parsed, is_view),
                RequestOpts::deadline(deadline),
            )
            .await?
    };
    let mut result = match resp.result {
        Value::Object(m) => m,
        other => {
            return Err(ToolError::internal(format!(
                "{method} returned a non-object result: {other}"
            )))
        }
    };
    let win = result
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::internal(format!("{method} result has no path")))?;
    // the notes are read off the bridge's stats before anything else touches the object
    let stats = result.get("stats");
    let blank = stats_blank(stats);
    let notes = render_notes(is_view, blank, state.unsaved_edits());

    // the POC's extras over the bridge's object: camera mode/lookAt and the timing split
    if let Some(camera) = result.get_mut("camera").and_then(Value::as_object_mut) {
        camera.insert("mode".into(), json!(parsed.cam.mode));
        if let Some(look_at) = parsed.cam.look_at {
            camera.insert("lookAt".into(), json!(look_at));
        }
    }
    if let Some(ms) = result.get("ms").cloned() {
        result.insert("renderMs".into(), ms.get("render").cloned().unwrap_or(Value::Null));
        if !is_view {
            result.insert("readbackMs".into(), ms.get("readback").cloned().unwrap_or(Value::Null));
        }
    }

    // the host path: mapped, and really a file. review-2 L4: only an *inline* render hard-fails
    // here — the render succeeded either way and the PNG is on the Windows side, so
    // `inline:false` answers the bridge's own result with `hostPath: null` and a note (and skips
    // the local pixel decode entirely: the bridge's `stats` are already in the reply) instead of
    // inviting a retry that would run the whole multi-second render again for the same path error.
    let mut images: Vec<Image> = Vec::new();
    result.insert("path".into(), json!(win));
    match mapped_host_file(&state, &win) {
        Some(host) if parsed.inline => {
            result.insert("hostPath".into(), json!(host.display().to_string()));
            // the image pipeline (particle_render's, shared with model_render): decode, measure
            // (`localStats`), inline per the size policy — only in this branch, an `inline:false`
            // render never decodes anything locally
            let (stats, inline) = render_pixels(&host, true).await?;
            result.insert("localStats".into(), stats.to_json());
            result.insert("inline".into(), Value::Null);
            if let Some(inline) = inline {
                result.insert(
                    "inline".into(),
                    json!({
                        "mime": IMAGE_MIME,
                        "width": inline.width,
                        "height": inline.height,
                        "downscaled": inline.downscaled,
                        "bytes": inline.png.len(),
                    }),
                );
                images.push(Image {
                    bytes: inline.png.clone(),
                    mime: IMAGE_MIME,
                });
            }
        }
        Some(host) => {
            result.insert("hostPath".into(), json!(host.display().to_string()));
            result.insert("inline".into(), Value::Null);
        }
        None if !parsed.inline => {
            result.insert("hostPath".into(), Value::Null);
            push_note(
                &mut result,
                format!(
                    "the render succeeded (stats inline above) but the image was not read back \
                     here — {win} has no mapped host file —, so hostPath is null and there is no \
                     localStats/inline image. Nothing is wrong with the render itself: a retry \
                     would render the same thing again for the same path result. Get the file by \
                     adding a --path-map entry that covers the bridge's renders directory (see \
                     the hint in the PathNotMapped error for `inline:true`)."
                ),
            );
        }
        None => {
            let tool = match state.path_map.windows_to_host(&win) {
                None => {
                    ToolError::new(
                        "PathNotMapped",
                        "PathNotMapped",
                        format!("no path-map entry covers {win}"),
                    )
                    .with_code(-32602)
                    .with_data(json!({
                        "path": win,
                        "hostPath": Value::Null,
                        "hint": "add --path-map 'z:\\app=<host dir>' (or set CS2_MCP_PATH_MAP); defaults cover z:\\app, z:\\cache and c:",
                    }))
                }
                Some(host) => {
                    ToolError::new(
                        "RenderFileMissing",
                        "RenderFileMissing",
                        format!(
                            "{method} reported {win} but {} is not a file: the render itself \
                             succeeded and the PNG is on the Windows side, so the path map must \
                             cover its renders directory exactly — a retry would render the \
                             same image again for the same error",
                            host.display()
                        ),
                    )
                    .with_data(json!({"path": win, "hostPath": host.display().to_string()}))
                }
            };
            return Err(tool);
        }
    }
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }
    Ok(ToolOutput {
        json: Value::Object(result),
        images,
        ..Default::default()
    })
}

/// The bridge's `stats` say the image is blank: no non-black pixel, or a luma range under 4.
pub(crate) fn stats_blank(stats: Option<&Value>) -> bool {
    stats.and_then(|s| s.get("nonBlackPixels")).and_then(Value::as_u64) == Some(0)
        || stats
            .and_then(|s| s.get("maxLuma"))
            .and_then(Value::as_u64)
            .zip(stats.and_then(|s| s.get("minLuma")).and_then(Value::as_u64))
            .is_some_and(|(max, min)| max.saturating_sub(min) < 4)
}

/// The mapped host path when it really is a file there, `None` when the path does not map or the
/// mapped place does not hold the file (review-2 L4 splits the answer only on `inline`).
fn mapped_host_file(state: &HammerState, win: &str) -> Option<std::path::PathBuf> {
    let host = state.path_map.windows_to_host(win)?;
    host.is_file().then_some(host)
}

/// Append one note to a result map's `notes` array (the mesh tools' shape).
fn push_note(result: &mut Map<String, Value>, note: String) {
    let notes = result
        .entry("notes".to_string())
        .or_insert_with(|| json!([]));
    if let Some(notes) = notes.as_array_mut() {
        notes.push(json!(note));
    }
}

/// The blank-image and live/compiled notes appended to the result; empty when there is nothing
/// to say.
fn render_notes(is_view: bool, blank: bool, unsaved: u64) -> Vec<String> {
    let mut notes = Vec::new();
    if blank {
        notes.push(if is_view {
            "the image looks blank (uniform colour): the camera may be inside geometry or \
             facing away from the map"
                .to_string()
        } else {
            "the image looks blank (uniform colour): the camera may be inside geometry or \
             outside the map, or no map is loaded in the engine (after `disconnect` the empty \
             scene world still renders, black)"
                .to_string()
        });
    }
    if unsaved > 0 {
        notes.push(if is_view {
            "the render shows the LIVE document, including unsaved edits".to_string()
        } else {
            "render_world shows the COMPILED map in the engine; unsaved Hammer edits are not \
             included (use render_view for the live document)"
                .to_string()
        });
    }
    notes
}

/// `render_view {camera?, width?, height?, fov?, znear?, zfar?, hideToolMaterials?, name?, inline?}`.
async fn render_view(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    run(state, ctx, args, "view.render", true).await
}

/// `render_world {camera, width?, height?, fov?, znear?, zfar?, name?, inline?}`.
async fn render_world(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    run(state, ctx, args, "world.render", false).await
}
