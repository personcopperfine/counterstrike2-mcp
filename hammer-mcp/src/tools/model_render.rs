//! `model_render` (ModelDoc vision pass, WP-T): render ModelDoc's main 3D viewport to a PNG
//! through the bridge (`modeldoc.render`, m2.14) and hand the pixels to the model.
//!
//! Same arguments, result shape and inline policy as [`super::particle_render::particle_render`]
//! (the shared pipeline is [`super::particle_render::run_render`], parameterised by the bridge
//! method's wire name); only the object chain differs: the render goes through the modeldoc chain
//! (`app ->activeSession -> main window -> scene widget -> tool widget -> scene + camera`), so an
//! active session — an open `.vmdl` tab in the running ModelDoc — is required (the chain failure
//! is `-32019 NoViewport`). The document itself is optional (`hasDocument` reports it). The file
//! lands in `Z:\app\renders\<stem>.png`, exactly where [`cs2_mcp_common::pathmap::PathMap`]'s
//! default `z:\app` entry maps to the host.
use super::particle_render::run_render;
use crate::state::HammerState;
use cs2_mcp_common::tools::{
    CallCtx, Mutability::ReadOnly, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Value};

/// The one tool this module registers, appended after the ten first-pass ModelDoc tools in
/// [`super::modeldoc::MODELDOC_TOOL_ORDER`].
pub const MODEL_RENDER_TOOL_ORDER: [&str; 1] = ["model_render"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "model_render",
            "Render ModelDoc's main 3D viewport to a PNG under <app>/renders/ and attach it as an image (downscaled above 1920x1080). Needs an open .vmdl tab in the running ModelDoc. Uses the viewport's own camera.",
            ReadOnly,
            &["modeldoc.render"],
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
        model_render,
    );
}

/// `model_render {width?, height?, name?, inline?}`: the shared render pipeline against
/// `modeldoc.render`.
pub async fn model_render(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    run_render(state, ctx, args, "modeldoc.render").await
}
