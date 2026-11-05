//! Vision tools (workflows/09-vision-ocr.md): `render_world` (bridge `world.render` -> PNG on the
//! host, stats, one inline image block), `ocr` (tesseract on the last capture or a PNG), and the
//! reusable [`capture_output`] that `screenshot` will call once the runtime exists.
//!
//! Every capture: the host PNG path is always returned; stats always; the image is inlined when
//! it is at most `INLINE_MAX_PIXELS` (1920x1080), otherwise a nearest-neighbour downscaled copy.
//! A frame that needs no downscale and no R/B swap is inlined as the original file bytes.
use crate::state::ClientState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::capture::frame::{self, ImageStats, InlinePolicy, MAX_PIXELS};
use cs2_mcp_common::capture::ocr::{self, OcrError, CROP_UPSCALE};
use cs2_mcp_common::tools::{
    CallCtx, Image, Mutability::ReadOnly, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const IMAGE_MIME: &str = "image/png";
/// OCR psm values this tool accepts (6 = uniform block, 11 = sparse text).
pub const OCR_PSMS: [u8; 2] = [6, 11];
const RENDER_WORLD_KEYS: [&str; 8] = [
    "origin", "angles", "width", "height", "fov", "znear", "zfar", "srgb",
];
const DEFAULT_RENDER_W: u64 = 1920;
const DEFAULT_RENDER_H: u64 = 1080;

pub fn render_world_spec() -> ToolSpec {
    ToolSpec::new(
        "render_world",
        "Offscreen freecam render of the loaded map (wine-tools bridge). Not for: the player's view (screenshot).",
        ReadOnly,
        &["world.render"],
    )
    .with_schema(json!({
        "type": "object",
        "properties": {
            "origin": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "angles": {"type": "array", "items": {"type": "number"}, "minItems": 3, "maxItems": 3},
            "width": {"type": "integer", "minimum": 16, "maximum": 8192, "description": "default 1920; width*height <= 33554432"},
            "height": {"type": "integer", "minimum": 16, "maximum": 8192},
            "fov": {"type": "number", "default": 90, "description": "horizontal degrees, (1,179)"},
            "znear": {"type": "number"},
            "zfar": {"type": "number"},
            "srgb": {"type": "boolean"},
            "name": {"type": "string"}
        },
        "required": ["origin", "angles"],
        "additionalProperties": false
    }))
}

pub fn ocr_spec() -> ToolSpec {
    ToolSpec::new(
        "ocr",
        "Extract text from the last capture (last: true) or a host PNG with tesseract.",
        ReadOnly,
        &[],
    )
    .with_schema(json!({
        "type": "object",
        "properties": {
            "png": {"type": "string"},
            "last": {"type": "boolean"},
            "crop": {"type": "array", "items": {"type": "integer", "minimum": 0}, "minItems": 4, "maxItems": 4, "description": "[x, y, w, h] pixels"},
            "psm": {"type": "integer", "enum": [6, 11], "default": 6}
        },
        "additionalProperties": false
    }))
}

// ---------------------------------------------------------------------------
// render_world
// ---------------------------------------------------------------------------

/// `render_world`: validate, forward the set keys to `world.render` (the C handler applies the
/// defaults and range checks), map the returned Windows path to the host, and attach the capture.
pub async fn render_world(
    state: ClientState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let params = render_world_params(&args)?;
    let resp = {
        // lock first: the deadline is computed from the budget the moment the request goes out
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        bridge
            .request(
                "world.render",
                Value::Object(params),
                RequestOpts::deadline(deadline),
            )
            .await?
    };
    let mut bridge_result = match resp.result {
        Value::Object(m) => m,
        other => {
            return Err(ToolError::internal(format!(
                "world.render returned a non-object result: {other}"
            )))
        }
    };
    let win = bridge_result
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::internal("world.render result has no path"))?;
    let host = state.path_map.windows_to_host(&win).ok_or_else(|| {
        ToolError::new(
            "PathNotMapped",
            "PathNotMapped",
            format!("no path-map entry covers {win}"),
        )
        .retry_safe(false)
        .with_data(json!({
            "windowsPath": win,
            "hint": "add --path-map 'z:\\app=<host dir>' (or set CS2_MCP_PATH_MAP); defaults cover z:\\app, z:\\cache and c:",
        }))
    })?;
    if !host.is_file() {
        return Err(ToolError::new(
            "ArtifactMissing",
            "ArtifactMissing",
            format!("bridge reported {win} but {} is not a file", host.display()),
        )
        .retry_safe(true)
        .with_data(json!({"windowsPath": win, "path": host.display().to_string()})));
    }
    // the bridge's own stats use a different (luma) measure; keep them, under another key
    let bridge_stats = bridge_result.remove("stats");
    bridge_result.remove("path");
    let mut meta = bridge_result;
    meta.insert("windowsPath".into(), json!(win));
    if let Some(s) = bridge_stats {
        meta.insert("bridgeStats".into(), s);
    }
    capture_output(&state, &host, false, meta).await
}

fn render_world_params(args: &Value) -> Result<Map<String, Value>, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !RENDER_WORLD_KEYS.contains(&k.as_str()) && k.as_str() != "name")
    {
        return Err(ToolError::invalid_arguments(format!(
            "unknown argument {bad:?}"
        )));
    }
    for key in ["origin", "angles"] {
        let v = obj
            .get(key)
            .ok_or_else(|| ToolError::invalid_arguments(format!("{key}: [x,y,z] required")))?;
        check_vec3(key, v)?;
    }
    for key in ["width", "height"] {
        if let Some(v) = obj.get(key) {
            v.as_u64()
                .filter(|n| (16..=8192).contains(n))
                .ok_or_else(|| ToolError::invalid_arguments(format!("{key}: integer 16..=8192")))?;
        }
    }
    let w = obj
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_RENDER_W);
    let h = obj
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_RENDER_H);
    if w * h > MAX_PIXELS {
        return Err(ToolError::invalid_arguments(format!(
            "width*height = {} exceeds {MAX_PIXELS} pixels",
            w * h
        )));
    }
    for key in ["fov", "znear", "zfar"] {
        if let Some(v) = obj.get(key) {
            if !v.is_number() {
                return Err(ToolError::invalid_arguments(format!(
                    "{key}: number expected"
                )));
            }
        }
    }
    if let Some(v) = obj.get("srgb") {
        if !v.is_boolean() {
            return Err(ToolError::invalid_arguments("srgb: boolean expected"));
        }
    }
    if let Some(v) = obj.get("name") {
        if !v.is_string() {
            return Err(ToolError::invalid_arguments("name: string expected"));
        }
    }
    // forwarded as given; the bridge fills defaults and enforces ranges (C handler: mcp_bridge.c op_world_render)
    Ok(obj.clone())
}

fn check_vec3(key: &str, v: &Value) -> Result<(), ToolError> {
    let ok = v
        .as_array()
        .is_some_and(|a| a.len() == 3 && a.iter().all(|x| x.as_f64().is_some_and(f64::is_finite)));
    if ok {
        Ok(())
    } else {
        Err(ToolError::invalid_arguments(format!(
            "{key}: [a,b,c] of three finite numbers"
        )))
    }
}

// ---------------------------------------------------------------------------
// ocr
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum OcrSource {
    Png(PathBuf),
    Last,
}

#[derive(Debug, PartialEq)]
struct OcrArgs {
    source: OcrSource,
    crop: Option<[u32; 4]>,
    psm: u8,
}

/// `ocr`: text from the last capture or a PNG (optionally cropped and upscaled). The tesseract
/// run is capped by the call's remaining budget.
pub async fn ocr(state: ClientState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let parsed = parse_ocr_args(&args)?;
    let png = match parsed.source {
        OcrSource::Png(p) => p,
        OcrSource::Last => state
            .last_capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                ToolError::invalid_arguments(
                    "no capture yet: call render_world (or screenshot) first, or pass png",
                )
            })?,
    };
    let remaining = ctx.budget.remaining();
    if remaining.is_zero() {
        return Err(ToolError::budget_exceeded(&ctx.budget));
    }
    let timeout = ocr::OCR_TIMEOUT.min(remaining);
    let result = match parsed.crop {
        None => ocr::ocr_png(&png, parsed.psm, timeout).await,
        Some(c) => ocr::ocr_region(&png, c, parsed.psm, timeout).await,
    }
    .map_err(ocr_tool_error)?;
    let mut out = json!({
        "text": result.text,
        "psm": result.psm,
        "engine": result.engine,
        "png": png.display().to_string(),
    });
    if let Some(c) = parsed.crop {
        out["crop"] = json!(c);
        out["upscale"] = json!(CROP_UPSCALE);
    }
    Ok(out.into())
}

/// Name each failure by where it came from: arguments, the capture file, or the engine.
fn ocr_tool_error(e: OcrError) -> ToolError {
    match e {
        OcrError::Invalid(m) => ToolError::invalid_arguments(m),
        OcrError::Unreadable(m) => ToolError::new("CaptureFailed", "CaptureFailed", m),
        OcrError::Engine(m) => ToolError::new("OcrFailed", "OcrFailed", m),
    }
}

fn parse_ocr_args(args: &Value) -> Result<OcrArgs, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !["png", "last", "crop", "psm"].contains(&k.as_str()))
    {
        return Err(ToolError::invalid_arguments(format!(
            "unknown argument {bad:?}"
        )));
    }
    let png = match obj.get("png") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(PathBuf::from(s.trim())),
        Some(_) => {
            return Err(ToolError::invalid_arguments(
                "png: non-empty host path string",
            ))
        }
    };
    let last = match obj.get("last") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ToolError::invalid_arguments("last: boolean")),
    };
    let source = match (png, last) {
        (Some(p), false) => OcrSource::Png(p),
        (None, true) => OcrSource::Last,
        (None, false) => {
            return Err(ToolError::invalid_arguments(
                "give either png (host path) or last: true",
            ))
        }
        (Some(_), true) => return Err(ToolError::invalid_arguments("png and last are exclusive")),
    };
    let psm = match obj.get("psm") {
        None | Some(Value::Null) => 6,
        Some(v) => v
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| OCR_PSMS.contains(n))
            .ok_or_else(|| ToolError::invalid_arguments("psm: 6 or 11"))?,
    };
    let crop = match obj.get("crop") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_crop(v)?),
    };
    Ok(OcrArgs { source, crop, psm })
}

fn parse_crop(v: &Value) -> Result<[u32; 4], ToolError> {
    let bad =
        || ToolError::invalid_arguments("crop: [x, y, w, h] non-negative integers, w and h > 0");
    let items = v.as_array().filter(|a| a.len() == 4).ok_or_else(bad)?;
    let mut c = [0u32; 4];
    for (slot, item) in c.iter_mut().zip(items) {
        *slot = item
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(bad)?;
    }
    if c[2] == 0 || c[3] == 0 {
        return Err(bad());
    }
    Ok(c)
}

// ---------------------------------------------------------------------------
// shared capture path (render_world now, screenshot later)
// ---------------------------------------------------------------------------

/// Decoded + policy-applied capture, computed off the async runtime.
#[derive(Debug)]
pub struct Captured {
    pub stats: ImageStats,
    /// `None` when the mode asked for no inline image
    pub inline: Option<InlinePolicy>,
}

/// What goes inline in a tool result. The full-size file is always reported as `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineMode {
    /// the policy default: the frame itself, downscaled only above 1920x1080
    Full,
    /// downscaled to at most this width (aspect kept); narrower frames are inlined as they are
    MaxWidth(u32),
    None,
}

/// Read, decode, optionally swap R/B (the game's `png_screenshot` writes BGR order), compute
/// stats and build the inline image per policy. Blocking; runs on the blocking pool.
pub fn process_capture(png: &Path, swap_rb: bool) -> Result<Captured, ToolError> {
    process_capture_mode(png, swap_rb, InlineMode::Full)
}

/// [`process_capture`] with an explicit inline mode.
pub fn process_capture_mode(
    png: &Path,
    swap_rb: bool,
    mode: InlineMode,
) -> Result<Captured, ToolError> {
    let bytes = std::fs::read(png).map_err(|e| {
        ToolError::new(
            "CaptureFailed",
            "CaptureFailed",
            format!("read {}: {e}", png.display()),
        )
    })?;
    let mut img = frame::decode_png(&bytes).map_err(|e| {
        ToolError::new(
            "CaptureFailed",
            "CaptureFailed",
            format!("{}: {e:#}", png.display()),
        )
    })?;
    // the source bytes are inlined untouched unless the pixels changed
    let original = if swap_rb {
        img.swap_rb();
        None
    } else {
        Some(bytes)
    };
    let stats = img.stats();
    let internal = |e: anyhow::Error| ToolError::internal(format!("{e:#}"));
    let inline = match mode {
        InlineMode::None => None,
        InlineMode::Full => Some(frame::inline_policy(&img, original).map_err(internal)?),
        InlineMode::MaxWidth(w) if img.width <= w => {
            Some(frame::inline_policy(&img, original).map_err(internal)?)
        }
        InlineMode::MaxWidth(w) => {
            let h = ((img.height as u64 * w as u64) / img.width as u64).max(1) as u32;
            let small = img.resize_nearest(w, h).map_err(internal)?;
            Some(InlinePolicy {
                png: small.to_png().map_err(internal)?,
                width: w,
                height: h,
                downscaled: true,
            })
        }
    };
    Ok(Captured { stats, inline })
}

/// Build the tool output for a host PNG: stats, one inline image block, `path`, and the
/// last-capture update. This is the reusable entry point (a `screenshot` tool passes
/// `swap_rb: true` for game `png_screenshot` files and its own `meta`).
pub async fn capture_output(
    state: &ClientState,
    png: &Path,
    swap_rb: bool,
    meta: Map<String, Value>,
) -> Result<ToolOutput, ToolError> {
    let owned = png.to_path_buf();
    let captured = tokio::task::spawn_blocking(move || process_capture(&owned, swap_rb))
        .await
        .map_err(|e| ToolError::internal(format!("capture task failed: {e}")))??;
    Ok(finish_capture(state, png, captured, meta))
}

/// Tool output for an already processed capture: sets `last_capture` (what `ocr {last}` reads),
/// adds `path`, `stats` and the inline descriptor, and carries the inline image block.
pub fn finish_capture(
    state: &ClientState,
    png: &Path,
    captured: Captured,
    mut meta: Map<String, Value>,
) -> ToolOutput {
    *state.last_capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(png.to_path_buf());
    meta.insert("path".into(), json!(png.display().to_string()));
    meta.insert("stats".into(), captured.stats.to_json());
    match captured.inline {
        Some(inline) => {
            meta.insert(
                "inline".into(),
                json!({
                    "mime": IMAGE_MIME,
                    "width": inline.width,
                    "height": inline.height,
                    "downscaled": inline.downscaled,
                    "bytes": inline.png.len(),
                }),
            );
            ToolOutput {
                json: Value::Object(meta),
                images: vec![Image {
                    bytes: inline.png,
                    mime: IMAGE_MIME,
                }],
                ..Default::default()
            }
        }
        None => {
            meta.insert("inline".into(), Value::Null);
            ToolOutput {
                json: Value::Object(meta),
                images: Vec::new(),
                ..Default::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_params_forward_and_validate() {
        let ok = render_world_params(
            &json!({"origin": [1, 2, 3], "angles": [0, 90, 0], "width": 640, "name": "a"}),
        );
        assert!(ok.is_ok());
        for bad in [
            json!({"angles": [0, 0, 0]}),
            json!({"origin": [1, 2], "angles": [0, 0, 0]}),
            json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "width": 8}),
            json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "srgb": "yes"}),
            json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "hideToolMaterials": true}),
            // 8192 x 8192 = 67M pixels > 33554432
            json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "width": 8192, "height": 8192}),
            json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "height": 8192, "width": 8192}),
        ] {
            let e = render_world_params(&bad).unwrap_err();
            assert_eq!(e.name, "InvalidParams", "{bad}");
        }
        assert!(render_world_params(
            &json!({"origin": [1, 2, 3], "angles": [0, 0, 0], "width": 8192, "height": 4096})
        )
        .is_ok());
    }

    #[test]
    fn ocr_args_are_validated() {
        assert_eq!(
            parse_ocr_args(&json!({"last": true})).unwrap(),
            OcrArgs {
                source: OcrSource::Last,
                crop: None,
                psm: 6
            }
        );
        let a = parse_ocr_args(&json!({"png": "/x.png", "crop": [1, 2, 3, 4], "psm": 11})).unwrap();
        assert_eq!(a.crop, Some([1, 2, 3, 4]));
        assert_eq!(a.psm, 11);
        for bad in [
            json!({}),
            json!({"png": "/x.png", "last": true}),
            json!({"last": false}),
            json!({"last": true, "psm": 7}),
            json!({"last": true, "crop": [1, 2, 3]}),
            json!({"last": true, "crop": [1, 2, 0, 4]}),
            json!({"last": true, "crop": [-1, 2, 3, 4]}),
            json!({"png": ""}),
            json!({"last": true, "lang": "eng"}),
        ] {
            let e = parse_ocr_args(&bad).unwrap_err();
            assert_eq!(e.name, "InvalidParams", "{bad}");
        }
    }

    #[test]
    fn ocr_errors_are_named_by_origin() {
        assert_eq!(
            ocr_tool_error(OcrError::Invalid("crop".into())).name,
            "InvalidParams"
        );
        assert_eq!(
            ocr_tool_error(OcrError::Unreadable("x".into())).type_,
            "CaptureFailed"
        );
        assert_eq!(
            ocr_tool_error(OcrError::Engine("x".into())).type_,
            "OcrFailed"
        );
    }
}
