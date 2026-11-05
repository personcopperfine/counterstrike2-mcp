//! `particle_render` (pet plan §0.2, WP-T2): render the particle editor's own preview viewport to
//! a PNG through the bridge (`pet.render`, m2.13) and hand the pixels to the model.
//!
//! Shape of a result: the bridge's object passed through (`path` = the Windows path on disk,
//! `file`, `width`, `height`, `bytes`, `format`, `hasDocument`, `stats`, `durationMs`) plus
//! `hostPath` (the mapped host file via `--path-map`,
//! [`cs2_mcp_common::pathmap::PathMap`]) and `localStats`
//! (the decoded pixels' own statistics, [`cs2_mcp_common::capture::frame::ImageStats`]); `inline`
//! carries `{mime,width,height,downscaled,bytes}` when the image block is attached and `null`
//! when the caller passed `inline:false`. The image pipeline is exactly the client's vision one
//! (`client-mcp/src/tools/vision.rs::finish_capture`): the file is inlined as-is up to
//! [`cs2_mcp_common::capture::INLINE_MAX_PIXELS`] (1920x1080) and downscaled above it. Unlike the
//! game's `png_screenshot`, `pet.render` writes RGBA8888 in display order, so no R/B swap.
//!
//! Policy (plan §0.2, "params first"): `width`/`height`/`name`/`inline` are validated before
//! anything is sent — an out-of-range or over-product width/height, or a name the bridge's
//! `render_name` would refuse, is `InvalidParams` with no request on the wire. A path the
//! `--path-map` does not cover is `PathNotMapped` (never guessed), a render the bridge wrote but
//! which is not there is `RenderFileMissing` (retry-safe: the bridge has already finished).
//! A bridge-side refusal (`-32019 NoViewport`, `-32020 RenderFailed`, …) surfaces with its wire
//! name through `ToolError::from`; nothing is retried here.
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::capture::frame::{self, ImageStats, InlinePolicy};
use cs2_mcp_common::tools::{
    CallCtx, Image, Mutability::ReadOnly, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The one inline block's MIME type (same constant as the client's vision tools).
pub const IMAGE_MIME: &str = "image/png";

/// The one tool this module registers. The orchestrator places these after the live tools in
/// `PET_TOOL_ORDER` (`tools/particles.rs`), which is written at merge time.
pub const PARTICLE_RENDER_TOOL_ORDER: [&str; 1] = ["particle_render"];

const DEFAULT_WIDTH: u64 = 1024;
const DEFAULT_HEIGHT: u64 = 768;
/// `pet.render`'s own bounds (`mcp_bridge_pet_render.inc`, the `render_name`/size rules of 0.1).
const MAX_SIDE: u64 = 4096;
const MAX_PIXELS: u64 = 8_388_608;
const NAME_MAX: usize = 64;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "particle_render",
            "Render the particle editor's preview viewport to a PNG under <app>/renders/ and attach it as an image (downscaled above 1920x1080). Uses pet's own preview camera; no document needed.",
            ReadOnly,
            &["pet.render"],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "width": {
                    "type": "integer", "minimum": 16, "maximum": 4096,
                    "description": "default 1024"
                },
                "height": {
                    "type": "integer", "minimum": 16, "maximum": 4096,
                    "description": "default 768; width*height <= 8388608"
                },
                "name": {
                    "type": "string",
                    "description": "file stem [A-Za-z0-9._-], max 64; default render_<tick>_<seq>"
                },
                "inline": {
                    "type": "boolean",
                    "description": "false = path only (reports inline:null)"
                }
            },
            "additionalProperties": false
        })),
        particle_render,
    );
}

// ---------------------------------------------------------------- arguments

/// The validated `particle_render` arguments; the bridge would apply the same defaults, but the
/// values are always sent explicitly so the PNG size is known before the call and the model gets
/// exactly the pixels it asked for.
struct RenderArgs {
    width: u64,
    height: u64,
    name: Option<String>,
    inline: bool,
}

fn parse_args(args: &Value) -> Result<RenderArgs, ToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ToolError::invalid_arguments("arguments must be an object"))?;
    if let Some(bad) = obj
        .keys()
        .find(|k| !["width", "height", "name", "inline"].contains(&k.as_str()))
    {
        return Err(ToolError::invalid_arguments(format!(
            "unknown argument {bad:?}"
        )));
    }
    let size = |key: &str, dflt: u64| -> Result<u64, ToolError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(dflt),
            Some(v) => v
                .as_u64()
                .filter(|n| (16..=MAX_SIDE).contains(n))
                .ok_or_else(|| {
                    ToolError::invalid_arguments(
                        "width/height: 16..4096 each, at most 8388608 pixels",
                    )
                }),
        }
    };
    let width = size("width", DEFAULT_WIDTH)?;
    let height = size("height", DEFAULT_HEIGHT)?;
    if width * height > MAX_PIXELS {
        return Err(ToolError::invalid_arguments(
            "width/height: 16..4096 each, at most 8388608 pixels",
        ));
    }
    let name = match obj.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let raw = v
                .as_str()
                .ok_or_else(|| ToolError::invalid_arguments("name: a string is required"))?;
            Some(render_name(raw)?)
        }
    };
    let inline = match obj.get("inline") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_arguments("inline: boolean expected"))?,
    };
    Ok(RenderArgs {
        width,
        height,
        name,
        inline,
    })
}

/// The bridge's `render_name` rule, checked locally so a bad name never reaches the wire: an
/// optional trailing `.png` (any case) is dropped, then `[A-Za-z0-9._-]`, 1..64 chars, no
/// leading dot. Byte-wise (a non-ASCII name is refused by the charset, not by a slice panic).
pub(crate) fn render_name(raw: &str) -> Result<String, ToolError> {
    let bad = || ToolError::invalid_arguments("name: [A-Za-z0-9._-], 1..64 chars, no leading dot");
    let bytes = raw.as_bytes();
    let end = if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".png") {
        bytes.len() - 4
    } else {
        bytes.len()
    };
    let base = &bytes[..end];
    let ok = !base.is_empty()
        && base.len() <= NAME_MAX
        && !base.starts_with(b".")
        && base
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    if !ok {
        return Err(bad());
    }
    Ok(String::from_utf8_lossy(base).into_owned())
}

// ---------------------------------------------------------------- particle_render

/// `particle_render {width?, height?, name?, inline?}`: validate, call the bridge render method,
/// map the returned Windows path to the host, read + decode the PNG and attach it per the inline
/// policy. [`run_render`] is the shared body (see its doc comment for the pipeline);
/// [`particle_render`] fixes the bridge method to `pet.render`.
pub async fn particle_render(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    run_render(state, ctx, args, "pet.render").await
}

/// The render pipeline `particle_render` and `model_render` share (the only difference is the
/// bridge method's wire name, `pet.render` m2.13 vs `modeldoc.render` m2.14, which reaches the
/// text of the surfaced errors): validate the args, call the bridge method, map the returned
/// Windows path through [`cs2_mcp_common::pathmap::PathMap`], check it exists on the host
/// (`RenderFileMissing`), decode, measure (`localStats`) and apply the inline policy.
pub(crate) async fn run_render(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
    method: &'static str,
) -> Result<ToolOutput, ToolError> {
    let parsed = parse_args(&args)?;
    let mut params = Map::new();
    params.insert("width".into(), json!(parsed.width));
    params.insert("height".into(), json!(parsed.height));
    if let Some(name) = &parsed.name {
        params.insert("name".into(), json!(name));
    }
    let resp = {
        // lock first: the deadline is computed from the budget the moment the request goes out
        let mut bridge = state.bridge.lock().await;
        let deadline = ctx.budget.deadline_ms()?;
        bridge
            .request(
                method,
                Value::Object(params),
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
    let host = state.path_map.windows_to_host(&win).ok_or_else(|| {
        ToolError::new(
            "PathNotMapped",
            "PathNotMapped",
            format!("no path-map entry covers {win}"),
        )
        .with_code(-32602)
        .retry_safe(true)
        .with_data(json!({
            "path": win,
            "hint": "add --path-map 'z:\\app=<host dir>' (or set CS2_MCP_PATH_MAP); defaults cover z:\\app, z:\\cache and c:",
        }))
    })?;
    if !host.is_file() {
        return Err(ToolError::new(
            "RenderFileMissing",
            "RenderFileMissing",
            format!(
                "{method} reported {win} but {} is not a file (the bridge's write failed after \
                 the render; the render itself completed, so a retry may succeed)",
                host.display()
            ),
        )
        .retry_safe(true)
        .with_data(json!({"path": win, "hostPath": host.display().to_string()})));
    }
    // the bridge's own stats use a different (luma) measure; keep them under their own key
    let bridge_stats = result.remove("stats");
    result.remove("path");
    let mut meta = result;
    meta.insert("path".into(), json!(win));
    meta.insert("hostPath".into(), json!(host.display().to_string()));
    if let Some(stats) = bridge_stats {
        meta.insert("stats".into(), stats);
    }
    if !parsed.inline {
        // measure the pixels but build no inline copy (vision.rs `InlineMode::None`)
        let (stats, _) = render_pixels(&host, false).await?;
        meta.insert("localStats".into(), stats.to_json());
        meta.insert("inline".into(), Value::Null);
        return Ok(Value::Object(meta).into());
    }
    // decoding re-encodes the pixels; blocking work runs on the blocking pool (vision.rs shape)
    let (stats, inline) = render_pixels(&host, true).await?;
    let inline = inline.expect("an inline copy was asked for");
    meta.insert("localStats".into(), stats.to_json());
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
    Ok(ToolOutput {
        json: Value::Object(meta),
        images: vec![Image {
            bytes: inline.png,
            mime: IMAGE_MIME,
        }],
        ..Default::default()
    })
}

/// Decode the rendered PNG and build the inline copy (or not). Runs on the blocking pool; the
/// original file bytes are inlined untouched when no downscale is needed (this route performs no
/// pixel transform, so they still match).
pub(crate) async fn render_pixels(
    path: &Path,
    want_inline: bool,
) -> Result<(ImageStats, Option<InlinePolicy>), ToolError> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || process_png(&owned, want_inline))
        .await
        .map_err(|error| ToolError::internal(format!("render task failed: {error}")))?
}

/// [`render_pixels`]' blocking half: read, decode, measure, and apply the shared inline policy.
pub(crate) fn process_png(
    path: &Path,
    want_inline: bool,
) -> Result<(ImageStats, Option<InlinePolicy>), ToolError> {
    let bytes = std::fs::read(path).map_err(|error| {
        ToolError::new(
            "RenderFileMissing",
            "RenderFileMissing",
            format!("read {}: {error}", path.display()),
        )
        .retry_safe(true)
        .with_data(json!({"hostPath": path.display().to_string()}))
    })?;
    let img = frame::decode_png(&bytes).map_err(|error| {
        ToolError::new(
            "RenderDecodeFailed",
            "RenderDecodeFailed",
            format!("{}: {error:#}", path.display()),
        )
        .retry_safe(true)
        .with_data(json!({"hostPath": path.display().to_string()}))
    })?;
    let stats = img.stats();
    let inline = match want_inline {
        true => Some(
            frame::inline_policy(&img, Some(bytes))
                .map_err(|error| ToolError::internal(format!("{error:#}")))?,
        ),
        false => None,
    };
    Ok((stats, inline))
}
