//! `look` (WP3.1): one tool to see the map, over the render primitives (`render_view`,
//! `render_world`, [`super::render`]). It frames a target (entity, point, box or the whole saved
//! map) from poses it computes itself, so an agent with no idea about cameras gets usable images.
//!
//! * Render-only: no document changes and no camera moves. Every view is an explicit camera passed
//!   to the render call (the bridge has no method that sets the Hammer viewport camera), so the
//!   user's viewport is exactly as it was on return, success or error: the result says
//!   `cameraRestored: true` and `cameraMoved: false`.
//! * `angles` / `tour` / `frames` / `player_eye` render with the ENGINE (`render_world`: the
//!   compiled map) by default; `source:"editor"` renders the same poses with Hammer's live
//!   document (`render_view`, unsaved edits and editor helpers included).
//! * Framing ([`fit_distance`]): the target box is bounded by a sphere and the camera sits
//!   outside it far enough that the sphere fits the narrower of the horizontal / vertical fov, so
//!   the whole box is in frame from any view direction (unit-tested by projecting its corners).
use super::entities_read::need_live;
use super::particle_render::render_name;
use super::render::{look_angles, orbit_origin, run, stats_blank};
use crate::state::HammerState;
use cs2_mcp_common::play;
use cs2_mcp_common::tools::{
    nearest_names, CallCtx, Image, Mutability::ReadOnly, NextStep, Registry, ToolError, ToolOutput,
    ToolSpec,
};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// Longest image side: default, and the schema bounds (small by default so a weak agent does not
/// flood its context; the max keeps every image inline-able, `INLINE_MAX_PIXELS` is 1920x1080).
const MAX_SIZE_DEFAULT: u64 = 640;
const MAX_SIZE_MIN: u64 = 64;
const MAX_SIZE_MAX: u64 = 1920;
/// Horizontal fov of the framed views (`render_world` and `render_view` both take horizontal).
const LOOK_FOV: f64 = 70.0;
/// Room around the bounding sphere.
const FIT_MARGIN: f64 = 1.1;
/// Smallest bounding radius framed (a degenerate box would put the camera inside znear).
const MIN_RADIUS: f64 = 16.0;
/// Half extent of the box around a point and of the box of an entity (an origin has no extent).
const POINT_HALF: f64 = 32.0;
const ENTITY_HALF: f64 = 64.0;
/// Camera height of a standing player when `game.local` has no `eye`.
const STAND_EYE: f64 = 64.0;
const COUNT_MIN: u64 = 2;
const COUNT_MAX: u64 = 8;
const TOUR_DEFAULT: u64 = 6;
const FRAMES_DEFAULT: u64 = 4;
const INTERVAL_DEFAULT_MS: u64 = 500;
const INTERVAL_MAX_MS: u64 = 5000;
const SUGGESTION_POOL: u64 = 500;
/// The longest view label (`tour_8_yaw315`); `<name>_<label>` must fit the 64-char file stem.
const LONGEST_LABEL: usize = 14;
const KEYS: &[&str] = &["target", "mode", "pose", "maxSize", "count", "intervalMs", "source", "name"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "look",
            "See the map: framed images of a target from poses it computes (no camera maths). Changes nothing; the Hammer camera is never moved (cameraRestored). \
             target: entity (targetname or nodeID; prefab nodeID too), point [x,y,z], box {min,max} or \"map\" (saved mesh extents, else entity origins). \
             mode: viewport (no target: the Hammer viewport as is); angles (with target: two corners and top); tour (count views around it); \
             frames (count images of one pose over time; needs a running game, run_map first); player_eye (the player's eye, or pose). \
             Engine renders show the COMPILED map; source editor renders the live document. Describe what you see in writing, then fix what is wrong.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "target": {"description": "targetname or nodeID, [x,y,z], {min:[..],max:[..]}, or \"map\""},
                "mode": {"type": "string", "enum": ["viewport", "angles", "tour", "frames", "player_eye"]},
                "pose": {"type": "object", "description": "frames/player_eye: camera {origin:[x,y,z], angles:[pitch,yaw,roll]}",
                         "properties": {"origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
                                        "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3}},
                         "required": ["origin", "angles"], "additionalProperties": false},
                "maxSize": {"type": "integer", "minimum": MAX_SIZE_MIN, "maximum": MAX_SIZE_MAX, "default": MAX_SIZE_DEFAULT,
                            "description": "longest image side in px"},
                "count": {"type": "integer", "minimum": COUNT_MIN, "maximum": COUNT_MAX,
                          "description": "tour views (6) or frames (4)"},
                "intervalMs": {"type": "integer", "minimum": 0, "maximum": INTERVAL_MAX_MS, "default": INTERVAL_DEFAULT_MS,
                               "description": "frames: pause between images"},
                "source": {"type": "string", "enum": ["engine", "editor"], "default": "engine"},
                "name": {"type": "string", "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$",
                         "description": "file stem; each view appends its label"}
            },
            "additionalProperties": false
        })),
        look,
    );
}

// ------------------------------------------------------------------------------------ framing

type V3 = [f64; 3];

/// A resolved target: an axis-aligned box and how it was found.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    kind: &'static str,
    name: Option<String>,
    node_id: Option<i64>,
    min: V3,
    max: V3,
    /// what a `"map"` box was taken from ("mesh extents" or "entity origins")
    framed: Option<&'static str>,
}

impl Target {
    fn center(&self) -> V3 {
        [
            (self.min[0] + self.max[0]) / 2.0,
            (self.min[1] + self.max[1]) / 2.0,
            (self.min[2] + self.max[2]) / 2.0,
        ]
    }
    /// Radius of the sphere around the box (half its diagonal).
    fn radius(&self) -> f64 {
        let c = self.center();
        ((self.max[0] - c[0]).powi(2) + (self.max[1] - c[1]).powi(2) + (self.max[2] - c[2]).powi(2)).sqrt()
    }
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("kind".into(), json!(self.kind));
        if let Some(f) = self.framed {
            m.insert("framedOn".into(), json!(f));
        }
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        if let Some(n) = self.node_id {
            m.insert("nodeID".into(), json!(n));
        }
        m.insert("box".into(), json!({"min": r1(self.min), "max": r1(self.max)}));
        m.insert("center".into(), json!(r1(self.center())));
        Value::Object(m)
    }
}

fn r1(v: V3) -> V3 {
    [(v[0] * 10.0).round() / 10.0, (v[1] * 10.0).round() / 10.0, (v[2] * 10.0).round() / 10.0]
}

/// How far from the center a camera must sit so a sphere of `radius` fits the narrower half
/// field of view: horizontal `hfov` degrees at `aspect` = width / height (so the vertical half
/// tangent is `tan(hfov/2) / aspect`).
fn fit_distance(radius: f64, hfov: f64, aspect: f64) -> f64 {
    let th = (hfov.to_radians() / 2.0).tan();
    let half = th.min(th / aspect).atan();
    radius / half.sin() * FIT_MARGIN
}

/// The view poses of `mode` for `target`: `(label, pitch, yaw)`; the camera looks along
/// (pitch, yaw) at the center (`pitch > 0` from above, yaw 0 looks along +X).
fn view_directions(mode: &str, count: u64) -> Vec<(String, f64, f64)> {
    if mode == "tour" {
        return (0..count)
            .map(|i| {
                let yaw = (45.0 + 360.0 * i as f64 / count as f64) % 360.0;
                (format!("tour_{}_yaw{}", i + 1, yaw.round() as i64), 20.0, yaw)
            })
            .collect();
    }
    vec![
        ("corner_sw".to_string(), 25.0, 45.0),
        ("corner_ne".to_string(), 25.0, 225.0),
        ("top".to_string(), 80.0, 45.0),
    ]
}

/// One camera: where it sits, how it looks, and what it needs to see this far.
#[derive(Debug, Clone, PartialEq)]
struct Pose {
    origin: V3,
    angles: V3,
    zfar: f64,
}

fn framed_pose(t: &Target, pitch: f64, yaw: f64, aspect: f64) -> Result<Pose, ToolError> {
    let c = t.center();
    let r = t.radius().max(MIN_RADIUS);
    let d = fit_distance(r, LOOK_FOV, aspect);
    let origin = orbit_origin(c, d, pitch, yaw);
    let angles = look_angles(origin, c)?;
    Ok(Pose { origin, angles, zfar: (d + r * 2.0).clamp(30_000.0, 1_000_000.0) })
}

/// Image size for `max_size` at 16:9.
fn image_size(max_size: u64) -> (u64, u64) {
    (max_size, ((max_size as f64 * 9.0 / 16.0).round() as u64).max(16))
}

// ------------------------------------------------------------------------------------ arguments

struct Args {
    target: Option<Value>,
    mode: &'static str,
    pose: Option<(V3, V3)>,
    max_size: u64,
    count: Option<u64>,
    interval_ms: u64,
    editor: bool,
    name: Option<String>,
}

fn int_arg(obj: &Map<String, Value>, key: &str, min: u64, max: u64) -> Result<Option<u64>, ToolError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| ToolError::invalid_arguments(format!("{key} must be an integer in {min}..={max}"))),
    }
}

fn v3_of(v: &Value, what: &str) -> Result<V3, ToolError> {
    let bad = || ToolError::invalid_arguments(format!("{what} must be [x, y, z] (3 finite numbers)"));
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0; 3];
    for (i, x) in a.iter().enumerate() {
        out[i] = x.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
    }
    Ok(out)
}

fn parse(args: &Value) -> Result<Args, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    if let Some(bad) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(ToolError::invalid_arguments(format!("unknown argument {bad:?}")));
    }
    let target = obj.get("target").filter(|v| !v.is_null()).cloned();
    let mode = match obj.get("mode").filter(|v| !v.is_null()) {
        None => {
            if target.is_some() {
                "angles"
            } else {
                "viewport"
            }
        }
        Some(Value::String(m)) => match m.as_str() {
            "viewport" => "viewport",
            "angles" => "angles",
            "tour" => "tour",
            "frames" => "frames",
            "player_eye" => "player_eye",
            other => {
                return Err(ToolError::invalid_arguments(format!(
                    "mode {other:?} is not one of viewport, angles, tour, frames, player_eye"
                )))
            }
        },
        Some(_) => return Err(ToolError::invalid_arguments("mode must be a string")),
    };
    let pose = match obj.get("pose").filter(|v| !v.is_null()) {
        None => None,
        Some(p) => {
            let o = p.as_object().ok_or_else(|| ToolError::invalid_arguments("pose must be {origin, angles}"))?;
            let origin = v3_of(o.get("origin").unwrap_or(&Value::Null), "pose.origin")?;
            let angles = v3_of(o.get("angles").unwrap_or(&Value::Null), "pose.angles")?;
            Some((origin, angles))
        }
    };
    match mode {
        "viewport" if target.is_some() || pose.is_some() => {
            return Err(ToolError::invalid_arguments(
                "mode viewport shows the Hammer viewport as it is: drop target/pose, or use mode angles",
            ))
        }
        "angles" | "tour" if target.is_none() => {
            return Err(ToolError::invalid_arguments(format!("mode {mode} needs a target")))
        }
        "angles" | "tour" if pose.is_some() => {
            return Err(ToolError::invalid_arguments(format!("mode {mode} computes its poses from target: drop pose")))
        }
        "player_eye" if target.is_some() => {
            return Err(ToolError::invalid_arguments(
                "mode player_eye looks from the player (or pose): drop target, or use mode angles",
            ))
        }
        "frames" if target.is_none() && pose.is_none() => {
            return Err(ToolError::invalid_arguments("mode frames needs a target or a pose"))
        }
        "frames" if target.is_some() && pose.is_some() => {
            return Err(ToolError::invalid_arguments("mode frames takes target or pose, not both"))
        }
        _ => {}
    }
    let max_size = int_arg(obj, "maxSize", MAX_SIZE_MIN, MAX_SIZE_MAX)?.unwrap_or(MAX_SIZE_DEFAULT);
    let count = int_arg(obj, "count", COUNT_MIN, COUNT_MAX)?;
    if count.is_some() && !matches!(mode, "tour" | "frames") {
        return Err(ToolError::invalid_arguments("count applies to modes tour and frames"));
    }
    let interval_ms = int_arg(obj, "intervalMs", 0, INTERVAL_MAX_MS)?;
    if interval_ms.is_some() && mode != "frames" {
        return Err(ToolError::invalid_arguments("intervalMs applies to mode frames"));
    }
    let editor = match obj.get("source").filter(|v| !v.is_null()) {
        None => false,
        Some(Value::String(s)) if s == "engine" => false,
        Some(Value::String(s)) if s == "editor" => true,
        Some(_) => return Err(ToolError::invalid_arguments("source must be \"engine\" or \"editor\"")),
    };
    if editor && mode == "frames" {
        return Err(ToolError::invalid_arguments(
            "mode frames reads the running game: source editor does not apply (the editor does not move)",
        ));
    }
    if editor && mode == "viewport" {
        return Err(ToolError::invalid_arguments("mode viewport is already the editor: drop source"));
    }
    let name = match obj.get("name").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => {
            let raw = v
                .as_str()
                .ok_or_else(|| ToolError::invalid_arguments("name: a string is required"))?;
            let stem = render_name(raw)?;
            if stem.len() + 1 + LONGEST_LABEL > 64 {
                return Err(ToolError::invalid_arguments(format!(
                    "name: at most {} characters (each view appends _<label> to the 64-character file stem)",
                    64 - 1 - LONGEST_LABEL
                )));
            }
            Some(stem)
        }
    };
    Ok(Args {
        target,
        mode,
        pose,
        max_size,
        count,
        interval_ms: interval_ms.unwrap_or(INTERVAL_DEFAULT_MS),
        editor,
        name,
    })
}

// ------------------------------------------------------------------------------------ target

fn box_around(c: V3, half: f64) -> (V3, V3) {
    ([c[0] - half, c[1] - half, c[2] - half], [c[0] + half, c[1] + half, c[2] + half])
}

/// Resolve `target` to a box with the existing readers (`get_entity`, `get_scene_info`).
async fn resolve_target(state: &HammerState, ctx: &CallCtx, target: &Value) -> Result<Target, ToolError> {
    match target {
        Value::Array(_) => {
            let p = v3_of(target, "target")?;
            let (min, max) = box_around(p, POINT_HALF);
            Ok(Target { kind: "point", name: None, node_id: None, min, max, framed: None })
        }
        Value::Object(o) => {
            let min = v3_of(o.get("min").unwrap_or(&Value::Null), "target.min")?;
            let max = v3_of(o.get("max").unwrap_or(&Value::Null), "target.max")?;
            if (0..3).any(|i| min[i] > max[i]) {
                return Err(ToolError::invalid_arguments("target box: min must not exceed max on any axis"));
            }
            if o.keys().any(|k| k != "min" && k != "max") {
                return Err(ToolError::invalid_arguments("target box takes min and max only"));
            }
            Ok(Target { kind: "box", name: None, node_id: None, min, max, framed: None })
        }
        Value::String(s) if s.trim().eq_ignore_ascii_case("map") => {
            let info = super::scene_info::get_scene_info(
                state.clone(),
                ctx.clone(),
                json!({"source": "saved", "limit": 1}),
            )
            .await?
            .json;
            // world geometry first: the point entities `new_map` keeps from the template (minimap
            // boundary, intro cameras, sky helpers) sit far from it and would loosen the frame
            let mesh_b = info.get("meshBounds").filter(|b| b.is_object());
            let framed = if mesh_b.is_some() { "mesh extents" } else { "entity origins" };
            let b = mesh_b.or_else(|| info.get("bounds").filter(|b| b.is_object()));
            let (Some(min), Some(max)) = (
                b.and_then(|b| v3_of(&b["min"], "bounds.min").ok()),
                b.and_then(|b| v3_of(&b["max"], "bounds.max").ok()),
            ) else {
                return Err(ToolError::new(
                    "EmptyMap",
                    "EmptyMap",
                    "the saved map has no entities or meshes to take bounds from; pass a box or a point",
                )
                .retry_safe(true));
            };
            Ok(Target { kind: "map", name: None, node_id: None, min, max, framed: Some(framed) })
        }
        Value::String(_) | Value::Number(_) => {
            // a string is a targetname; a string of digits is also tried as a nodeID
            let (tries, name): (Vec<(Value, Option<i64>)>, Option<String>) = match target {
                Value::String(s) => {
                    let mut t = vec![(json!({"targetname": s}), None)];
                    if let Some(id) = s.trim().parse::<i64>().ok().filter(|id| *id >= 0) {
                        t.push((json!({"nodeID": id}), Some(id)));
                    }
                    (t, Some(s.clone()))
                }
                n => match n.as_i64() {
                    Some(id) if id >= 0 => (vec![(json!({"nodeID": id}), Some(id))], None),
                    _ => return Err(ToolError::invalid_arguments("target nodeID must be a non-negative integer")),
                },
            };
            let mut found: Option<(Value, Option<i64>)> = None;
            let mut last_err: Option<ToolError> = None;
            for (args, id) in tries {
                match super::entities_read::get_entity(state.clone(), ctx.clone(), args).await {
                    Ok(out) => {
                        found = Some((out.json, id));
                        break;
                    }
                    Err(e) if e.name == "EntityNotFound" || e.name == "NotAnEntity" => last_err = Some(e),
                    Err(e) => return Err(e),
                }
            }
            let Some((reply, node_id)) = found else {
                let e = last_err.expect("one try at least");
                // a prefab node is not an entity: the saved file knows it (`CMapPrefab`)
                let id = match target {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse::<i64>().ok(),
                    _ => None,
                };
                if let Some(id) = id.filter(|id| *id >= 0) {
                    if let Some(t) = prefab_target(state, ctx, id).await {
                        return Ok(t);
                    }
                }
                if e.name == "NotAnEntity" {
                    return Err(ToolError::new(
                        "NotAnEntity",
                        "NotAnEntity",
                        format!("{}; a prefab node is only known once saved: save_map, then look again", e.message),
                    )
                    .retry_safe(true));
                }
                return Err(not_found(state, ctx, e, name.as_deref()).await);
            };
            let name = if node_id.is_some() && matches!(target, Value::String(_)) { None } else { name };
            let origin = reply
                .get("entity")
                .unwrap_or(&reply)
                .get("origin")
                .and_then(|o| v3_of(o, "origin").ok())
                .ok_or_else(|| {
                    ToolError::new("NoOrigin", "NoOrigin", "the entity has no origin to look at; pass a point or a box")
                        .retry_safe(true)
                })?;
            let (min, max) = box_around(origin, ENTITY_HALF);
            Ok(Target { kind: "entity", name, node_id, min, max, framed: None })
        }
        _ => Err(ToolError::invalid_arguments(
            "target must be a targetname, a nodeID, [x,y,z], {min,max} or \"map\"",
        )),
    }
}

/// A `CMapPrefab` node of the SAVED map (the live `entity.get` refuses it) as a box of the same
/// +-64 rule as a point entity around its origin: the prefab's contents bounds are not read.
async fn prefab_target(state: &HammerState, ctx: &CallCtx, node_id: i64) -> Option<Target> {
    let (_, document, _, _) = super::entities_file::resolve_document(state, ctx, &json!({})).await.ok()?;
    let origin = document.prefabs().iter().find(|p| p.node_id == Some(node_id))?.origin?;
    let (min, max) = box_around(origin, ENTITY_HALF);
    Some(Target { kind: "prefab", name: None, node_id: Some(node_id), min, max, framed: Some("prefab origin +-64") })
}

/// An unknown entity: the reader's error plus `didYouMean` from the NAMED entities that share a
/// word (or the first letters) with the request. `get_scene_info {query}` filters on the whole
/// map, so the pool is not the first rows of an unnamed-heavy listing.
async fn not_found(state: &HammerState, ctx: &CallCtx, e: ToolError, name: Option<&str>) -> ToolError {
    let Some(name) = name else { return e };
    let mut queries: Vec<String> = name
        .split(['_', '.', '-', ' '])
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_ascii_lowercase)
        .collect();
    queries.sort_by_key(|w| std::cmp::Reverse(w.len()));
    queries.truncate(2);
    let prefix: String = name.chars().take(3).collect::<String>().to_ascii_lowercase();
    if prefix.chars().count() == 3 && !queries.contains(&prefix) {
        queries.push(prefix);
    }
    let mut names: Vec<String> = Vec::new();
    for q in queries {
        let listing = super::scene_info::get_scene_info(
            state.clone(),
            ctx.clone(),
            json!({"query": q, "limit": SUGGESTION_POOL, "fields": ["targetname"]}),
        )
        .await;
        let Ok(listing) = listing else { continue };
        for i in listing.json.get("items").and_then(Value::as_array).into_iter().flatten() {
            if let Some(n) = i.get("targetname").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                if !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            }
        }
    }
    let near = nearest_names(name, names.iter().map(String::as_str), 5);
    e.did_you_mean(near)
}

// ------------------------------------------------------------------------------------ rendering

/// One finished view, ready for the result.
struct View {
    label: String,
    pose: Value,
    detail: Map<String, Value>,
    image: Option<Image>,
    notes: Vec<String>,
    blank: bool,
}

/// Render one camera through `run` (the shared render pipeline) and keep what an agent needs.
#[allow(clippy::too_many_arguments)]
async fn render_one(
    state: &HammerState,
    ctx: &CallCtx,
    args: &Args,
    label: &str,
    origin: Option<V3>,
    angles: Option<V3>,
    zfar: Option<f64>,
    fov: Option<f64>,
) -> Result<View, ToolError> {
    let (w, h) = image_size(args.max_size);
    let mut wire = Map::new();
    wire.insert("width".into(), json!(w));
    wire.insert("height".into(), json!(h));
    wire.insert("inline".into(), json!(true));
    if let Some(o) = origin {
        wire.insert("origin".into(), json!(o));
    }
    if let Some(a) = angles {
        wire.insert("angles".into(), json!(a));
    }
    if let Some(z) = zfar {
        wire.insert("zfar".into(), json!(z));
    }
    if let Some(f) = fov {
        wire.insert("fov".into(), json!(f));
    }
    if let Some(n) = &args.name {
        wire.insert("name".into(), json!(format!("{n}_{label}")));
    }
    let (method, is_view) = if args.editor || args.mode == "viewport" {
        ("view.render", true)
    } else {
        ("world.render", false)
    };
    let mut out = run(state.clone(), ctx.clone(), Value::Object(wire), method, is_view).await?;
    let mut detail = Map::new();
    for k in ["file", "hostPath", "path"] {
        if let Some(v) = out.json.get(k) {
            detail.insert(k.into(), v.clone());
        }
    }
    let notes: Vec<String> = out
        .json
        .get("notes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|n| n.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    // the viewport view reports the camera the bridge used
    let blank = stats_blank(out.json.get("stats"));
    let pose = match origin {
        // an engine render defaults to fov 90; the editor uses the viewport's, which is not known here
        Some(o) => match fov.or(if is_view { None } else { Some(90.0) }) {
            Some(f) => json!({"origin": o, "angles": angles, "fov": f}),
            None => json!({"origin": o, "angles": angles, "fov": "the viewport's"}),
        },
        None => out.json.get("camera").cloned().unwrap_or(Value::Null),
    };
    detail.insert("size".into(), json!([w, h]));
    Ok(View { label: label.to_string(), pose, detail, image: out.images.pop(), notes, blank })
}

/// The Hammer-side requirement of a render route, checked before anything is sent (`run` is
/// called directly, so the registry's capability gate does not cover it).
async fn need_render(state: &HammerState, ctx: &CallCtx, editor: bool, viewport: bool) -> Result<(), ToolError> {
    need_live(state, ctx, &[if editor || viewport { "view.render" } else { "world.render" }]).await
}

/// The running player's eye: `game.local` -> (origin, angles) and where they came from.
async fn player_eye(state: &HammerState, ctx: &CallCtx) -> Result<(V3, V3, String), ToolError> {
    need_live(state, ctx, &["game.local"]).await?;
    let mut bridge = state.bridge.lock().await;
    let local = play::state::read(&mut bridge, ctx).await.map_err(super::game_live::no_map)?;
    if local.get("inGame").and_then(Value::as_bool) == Some(false) {
        return Err(super::game_live::no_map_error());
    }
    let player = local.get("player").cloned().unwrap_or(Value::Null);
    let (origin, how) = match player.get("eye").and_then(|e| v3_of(e, "eye").ok()) {
        Some(e) => (e, "game.local player.eye".to_string()),
        None => match player.get("origin").and_then(|o| v3_of(o, "origin").ok()) {
            Some(o) => ([o[0], o[1], o[2] + STAND_EYE], "player origin + 64 (no eye in game.local)".to_string()),
            None => {
                return Err(ToolError::new(
                    "NoPlayerPose",
                    "NoPlayerPose",
                    "game.local has no player origin (dead or not spawned): pass pose, or run_map with team ct/t",
                )
                .retry_safe(true)
                .use_instead("run_map", "spawn the player alive: run_map {team:\"ct\"}", None))
            }
        },
    };
    let found = local
        .get("viewAngles")
        .and_then(|a| v3_of(a, "viewAngles").ok())
        .or_else(|| player.get("eyeAngles").and_then(|a| v3_of(a, "eyeAngles").ok()));
    let (angles, how) = match found {
        Some(a) => (a, how),
        None => ([0.0, 0.0, 0.0], format!("{how}; NO view angles in game.local, angles [0,0,0] (looking along +X)")),
    };
    Ok((origin, angles, how))
}

pub async fn look(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let a = parse(&args)?;
    let (w, h) = image_size(a.max_size);
    let aspect = w as f64 / h as f64;
    let mut target_json = Value::Null;
    let mut views: Vec<View> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let mut first_error: Option<ToolError> = None;
    let mut how: Option<String> = None;

    // what to render: (label, origin, angles, zfar, fov); None origin = the viewport camera
    type Plan = (String, Option<V3>, Option<V3>, Option<f64>, Option<f64>);
    let mut plans: Vec<Plan> = Vec::new();
    let mut frame_count = 0u64;
    let target = match &a.target {
        Some(t) => Some(resolve_target(&state, &ctx, t).await?),
        None => None,
    };
    if let Some(t) = &target {
        target_json = t.to_json();
    }
    match a.mode {
        "viewport" => plans.push(("viewport".into(), None, None, None, None)),
        "angles" | "tour" => {
            let t = target.as_ref().expect("checked in parse");
            let count = a.count.unwrap_or(TOUR_DEFAULT);
            for (label, pitch, yaw) in view_directions(a.mode, count) {
                let p = framed_pose(t, pitch, yaw, aspect)?;
                plans.push((label, Some(p.origin), Some(p.angles), Some(p.zfar), Some(LOOK_FOV)));
            }
        }
        "frames" => {
            frame_count = a.count.unwrap_or(FRAMES_DEFAULT);
            // the game first: a refusal here sends no render
            need_live(&state, &ctx, &["game.local"]).await?;
            {
                let mut bridge = state.bridge.lock().await;
                let local = play::state::read(&mut bridge, &ctx).await.map_err(super::game_live::no_map)?;
                if local.get("inGame").and_then(Value::as_bool) == Some(false) {
                    return Err(super::game_live::no_map_error());
                }
            }
            let (origin, angles, zfar, fov) = match (&target, a.pose) {
                (Some(t), _) => {
                    let (_, pitch, yaw) = view_directions("angles", 0)[0].clone();
                    let p = framed_pose(t, pitch, yaw, aspect)?;
                    (p.origin, p.angles, Some(p.zfar), Some(LOOK_FOV))
                }
                (None, Some((o, an))) => (o, an, None, None),
                (None, None) => unreachable!("checked in parse"),
            };
            for i in 0..frame_count {
                plans.push((format!("frame_{}", i + 1), Some(origin), Some(angles), zfar, fov));
            }
        }
        _ => {
            let (origin, angles, source) = match a.pose {
                Some((o, an)) => (o, an, "pose argument".to_string()),
                None => player_eye(&state, &ctx).await?,
            };
            how = Some(source);
            plans.push(("player_eye".into(), Some(origin), Some(angles), None, None));
        }
    }
    need_render(&state, &ctx, a.editor, a.mode == "viewport").await?;

    // `stop` = why the loop ended early; everything planned and not attempted is then listed in `failed`
    let mut stop: Option<(String, usize)> = None;
    for (i, (label, origin, angles, zfar, fov)) in plans.iter().enumerate() {
        if a.mode == "frames" && i > 0 && a.interval_ms > 0 {
            let pause = Duration::from_millis(a.interval_ms);
            if ctx.budget.remaining() < pause + Duration::from_secs(5) {
                stop = Some(("the tool time budget is used up".to_string(), i));
                break;
            }
            tokio::time::sleep(pause).await;
        }
        match render_one(&state, &ctx, &a, label, *origin, *angles, *zfar, *fov).await {
            Ok(v) => views.push(v),
            Err(e) => {
                failed.push(json!({"label": label, "error": e.message, "type": e.name}));
                // deterministic or global: another try renders the same failure again
                let stop_here = matches!(
                    e.name.as_str(),
                    "ToolBudgetExceeded" | "NotImplemented" | "BridgeBusy" | "PathNotMapped" | "RenderFileMissing"
                );
                if stop_here {
                    stop = Some((format!("{}: {}", e.name, e.message), i + 1));
                }
                if first_error.is_none() {
                    first_error = Some(e);
                }
                if stop_here {
                    break;
                }
            }
        }
    }
    if let Some((cause, from)) = &stop {
        for (label, ..) in &plans[*from..] {
            failed.push(json!({"label": label, "error": format!("not rendered: {cause}")}));
        }
    }
    if views.is_empty() {
        // nothing rendered: the render error itself, never an empty success
        return Err(first_error.unwrap_or_else(|| {
            ToolError::new("NothingRendered", "NothingRendered", "look produced no image").retry_safe(true)
        }));
    }

    // blank images: a black engine render is "no compiled map loaded" far more often than a
    // real view of nothing
    let blank_labels: Vec<String> = views.iter().filter(|v| v.blank).map(|v| v.label.clone()).collect();
    let engine = !(a.editor || a.mode == "viewport");
    if engine && blank_labels.len() == views.len() {
        let mut retry = args.clone();
        let (tool_args, instead) = if a.mode == "frames" {
            (None, ("run_map", "load the compiled map in the engine, then look again"))
        } else {
            retry["source"] = json!("editor");
            (Some(retry), ("look", "render the live document in Hammer instead"))
        };
        let mut e = ToolError::new(
            "BlankRender",
            "BlankRender",
            "every engine view is blank (uniform colour): no compiled map is loaded in the engine \
             (editing before compile_map + run_map), or the camera is inside geometry. The engine \
             shows the COMPILED map, not the open document",
        )
        .retry_safe(true)
        .with_data(json!({
            "blankViews": blank_labels,
            "files": views.iter().filter_map(|v| v.detail.get("hostPath").cloned()).collect::<Vec<_>>(),
        }));
        e = e.use_instead(instead.0, instead.1, tool_args);
        return Err(e);
    }

    let mut notes: Vec<String> = Vec::new();
    if !blank_labels.is_empty() {
        notes.push(format!(
            "WARNING: {} of {} views are blank ({}): the camera may be inside geometry or outside the map",
            blank_labels.len(),
            views.len(),
            blank_labels.join(", ")
        ));
    }
    for v in &views {
        for n in &v.notes {
            if !notes.contains(n) {
                notes.push(n.clone());
            }
        }
    }
    if !failed.is_empty() {
        notes.push(format!(
            "{} of {} views failed or were not rendered (see failed); the images that did render are returned",
            failed.len(),
            plans.len()
        ));
    }
    let mut images = Vec::new();
    let mut view_json = Vec::new();
    for v in views {
        let mut m = Map::new();
        m.insert("label".into(), json!(v.label));
        m.insert("pose".into(), v.pose);
        m.extend(v.detail);
        m.insert("imageIndex".into(), v.image.as_ref().map_or(Value::Null, |_| json!(images.len())));
        images.extend(v.image);
        view_json.push(Value::Object(m));
    }
    let mut result = Map::new();
    result.insert("mode".into(), json!(a.mode));
    result.insert(
        "renderer".into(),
        json!(if a.editor || a.mode == "viewport" { "editor (render_view)" } else { "engine (render_world)" }),
    );
    result.insert("target".into(), target_json);
    result.insert("views".into(), Value::Array(view_json));
    result.insert("cameraRestored".into(), json!(true));
    result.insert("cameraMoved".into(), json!(false));
    result.insert("maxSize".into(), json!(a.max_size));
    if let Some(h) = how {
        result.insert("poseFrom".into(), json!(h));
    }
    if frame_count > 0 {
        result.insert("intervalMs".into(), json!(a.interval_ms));
    }
    if !blank_labels.is_empty() {
        result.insert("blankViews".into(), json!(blank_labels));
    }
    if !failed.is_empty() {
        result.insert("failed".into(), Value::Array(failed));
    }
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }
    let mut out = ToolOutput { json: Value::Object(result), images, next: None };
    let mut again = args.clone();
    let mut steps = vec![NextStep::new(
        "set_node_transform",
        "describe in writing what the images show and what is wrong; move or rotate what is misplaced",
    )];
    if a.mode == "frames" {
        steps.push(NextStep::new("compile_map", "frames show the running (compiled) map: after edits compile_map, run_map, then look again"));
    } else if engine {
        again["source"] = json!("editor");
        steps.push(
            NextStep::new(
                "look",
                "after document edits look again with source \"editor\": the engine shows the COMPILED map and would show the old one",
            )
            .args(again),
        );
        steps.push(NextStep::new("compile_map", "to see edits in the engine: compile_map, then run_map, then look"));
    } else {
        steps.push(NextStep::new("look", "after document edits look again to check the change").args(again));
    }
    out.next = Some(steps);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Project the 8 corners of `t` through a camera at `pose` (Source angles: pitch>0 looks
    /// down, yaw 0 = +X) and check they are all in front and inside both half-fovs.
    fn box_in_frame(t: &Target, pose: &Pose, hfov: f64, aspect: f64) -> bool {
        let (p, y) = (pose.angles[0].to_radians(), pose.angles[1].to_radians());
        let f = [p.cos() * y.cos(), p.cos() * y.sin(), -p.sin()];
        let right = {
            let n = f[1].hypot(f[0]);
            [f[1] / n, -f[0] / n, 0.0]
        };
        let up = [
            right[1] * f[2] - right[2] * f[1],
            right[2] * f[0] - right[0] * f[2],
            right[0] * f[1] - right[1] * f[0],
        ];
        let th = (hfov.to_radians() / 2.0).tan();
        let tv = th / aspect;
        let dot = |a: V3, b: V3| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        for i in 0..8 {
            let c = [
                if i & 1 == 0 { t.min[0] } else { t.max[0] },
                if i & 2 == 0 { t.min[1] } else { t.max[1] },
                if i & 4 == 0 { t.min[2] } else { t.max[2] },
            ];
            let d = [c[0] - pose.origin[0], c[1] - pose.origin[1], c[2] - pose.origin[2]];
            let z = dot(d, f);
            if z <= 4.0 || dot(d, right).abs() / z > th || dot(d, up).abs() / z > tv {
                return false;
            }
        }
        true
    }

    fn target(min: V3, max: V3) -> Target {
        Target { kind: "box", name: None, node_id: None, min, max, framed: None }
    }

    #[test]
    fn fit_distance_matches_hand_numbers() {
        // radius 100, hfov 90 at 16:9: the vertical half tangent is 1/(16/9) = 0.5625 (< 1), so
        // the vertical fov limits: half angle atan(0.5625) = 29.358 deg, sin = 0.4905
        let d = fit_distance(100.0, 90.0, 16.0 / 9.0);
        assert!((d - 100.0 / 0.49029 * FIT_MARGIN).abs() < 0.5, "{d}");
        // a square image is limited by the horizontal fov (hfov 60: sin(30) = 0.5)
        let d = fit_distance(100.0, 60.0, 1.0);
        assert!((d - 200.0 * FIT_MARGIN).abs() < 1e-9, "{d}");
    }

    #[test]
    fn every_angle_and_tour_view_has_the_whole_box_in_frame() {
        let (w, h) = image_size(640);
        assert_eq!((w, h), (640, 360));
        let aspect = w as f64 / h as f64;
        for t in [
            target([-50.0, -50.0, 0.0], [50.0, 50.0, 100.0]),
            target([0.0, 0.0, 0.0], [4096.0, 256.0, 64.0]),
            target([-2000.0, -3000.0, -200.0], [5000.0, 2500.0, 900.0]),
            target([10.0, 10.0, 10.0], [10.0, 10.0, 10.0]),
        ] {
            for (mode, count) in [("angles", 0), ("tour", 4), ("tour", 8)] {
                let dirs = view_directions(mode, count);
                assert_eq!(dirs.len(), if mode == "angles" { 3 } else { count as usize });
                for (label, pitch, yaw) in dirs {
                    let pose = framed_pose(&t, pitch, yaw, aspect).unwrap();
                    assert!(box_in_frame(&t, &pose, LOOK_FOV, aspect), "{label} of {t:?}: {pose:?}");
                    assert!(pose.zfar >= 30_000.0 && pose.zfar <= 1_000_000.0);
                    // the camera looks at the center: the center is straight ahead
                    let c = t.center();
                    let back = orbit_origin(c, 1.0, pitch, yaw);
                    assert!((back[2] - (c[2] + pitch.to_radians().sin())).abs() < 1e-2, "{label}");
                }
            }
        }
    }

    #[test]
    fn a_box_that_is_not_framed_fails_the_checker() {
        // the checker itself: a camera too close to a big box must fail
        let t = target([-500.0, -500.0, 0.0], [500.0, 500.0, 500.0]);
        let pose = Pose { origin: [-300.0, -300.0, 100.0], angles: [10.0, 45.0, 0.0], zfar: 30_000.0 };
        assert!(!box_in_frame(&t, &pose, LOOK_FOV, 16.0 / 9.0));
    }

    #[test]
    fn view_labels_and_yaws() {
        let tour = view_directions("tour", 4);
        let yaws: Vec<f64> = tour.iter().map(|v| v.2).collect();
        assert_eq!(yaws, vec![45.0, 135.0, 225.0, 315.0]);
        assert_eq!(tour[0].0, "tour_1_yaw45");
        let angles = view_directions("angles", 0);
        assert_eq!(
            angles.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
            vec!["corner_sw", "corner_ne", "top"]
        );
    }

    #[test]
    fn max_size_maps_to_a_16_9_image() {
        assert_eq!(image_size(640), (640, 360));
        assert_eq!(image_size(64), (64, 36));
        assert_eq!(image_size(1920), (1920, 1080));
    }

    #[test]
    fn arguments_are_validated() {
        assert_eq!(parse(&json!({})).unwrap().mode, "viewport");
        assert_eq!(parse(&json!({"target": "map"})).unwrap().mode, "angles");
        for bad in [
            json!({"mode": "angles"}),
            json!({"mode": "viewport", "target": "x"}),
            json!({"mode": "player_eye", "target": "x"}),
            json!({"mode": "frames"}),
            json!({"target": "x", "count": 3}),
            json!({"target": "x", "intervalMs": 10}),
            json!({"target": "x", "pose": {"origin": [0, 0, 0], "angles": [0, 0, 0]}}),
            json!({"target": "x", "maxSize": 32}),
            json!({"target": "x", "maxSize": 4000}),
            json!({"target": "x", "mode": "orbit"}),
            json!({"mode": "viewport", "source": "editor"}),
            json!({"mode": "frames", "target": "x", "source": "editor"}),
            json!({"bogus": 1}),
            json!({"mode": "player_eye", "pose": {"origin": [0, 0], "angles": [0, 0, 0]}}),
        ] {
            assert_eq!(parse(&bad).err().map(|e| e.name), Some("InvalidParams".to_string()), "{bad}");
        }
        let p = parse(&json!({"mode": "player_eye", "pose": {"origin": [1, 2, 3], "angles": [4, 5, 6]}})).unwrap();
        assert_eq!(p.pose, Some(([1.0, 2.0, 3.0], [4.0, 5.0, 6.0])));
        assert_eq!(p.max_size, MAX_SIZE_DEFAULT);
    }
}
