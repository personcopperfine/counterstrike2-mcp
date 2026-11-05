//! ModelDoc tools (`.vmdl` models, props, player models), appended after the 24 POC tools in
//! [`MODELDOC_TOOL_ORDER`]. Specs (name, description, mutability, bridge methods) live here so the
//! order and the gating are fixed in one place; handler bodies live in `model_file.rs` (file
//! route: read/list/edit/create/compile) and `model_live.rs` (the running editor through the
//! bridge). Plan: hammer-mcp/workflows/04-modeldoc.md, RE: research/modeldoc-re.md.
use super::{model_file, model_import, model_live, model_render};
use crate::state::HammerState;
use cs2_mcp_common::tools::{Mutability::*, Registry, ToolSpec};

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "model_status",
            "ModelDoc editor status: whether modeldoc_editor.dll is loaded, missing capabilities, the active document (+ host path, inAddon) and a launch/open hint. Never errors.",
            ReadOnly,
            &[],
        ),
        model_live::model_status,
    );
    r.register(
        ToolSpec::new(
            "model_read",
            "Read a .vmdl from disk: one node subtree as JSON (_class, name, attrs, children). Path: host, Z:\\app\\..., or csgo_addons/<addon>/models/x.vmdl.",
            ReadOnly,
            &[],
        ),
        model_file::model_read,
    );
    r.register(
        ToolSpec::new(
            "model_list_nodes",
            "Flat node list of a .vmdl on disk (path /i/j, _class, name, childCount, attrKeys), filterable by class/name.",
            ReadOnly,
            &[],
        ),
        model_file::model_list_nodes,
    );
    r.register(
        ToolSpec::new(
            "model_edit",
            "Edit a .vmdl in the active addon: ops add_node/remove_node/move_node/set_attr/remove_attr, all-or-nothing, validated, backup + atomic write. Does not touch the editor: model_reload afterwards.",
            Writer,
            &[],
        ),
        model_file::model_edit,
    );
    r.register(
        ToolSpec::new(
            "model_create",
            "Create a new .vmdl in the active addon from a template (static_prop, skeletal). Refuses an existing file unless overwrite.",
            Writer,
            &[],
        ),
        model_file::model_create,
    );
    r.register(
        ToolSpec::new(
            "model_compile",
            "Compile a .vmdl to .vmdl_c. route resourcecompiler (default; works for new files) or editor (needs hasAsset; refuses a dirty document unless saveFirst). ok = .vmdl_c updated.",
            Writer,
            &[],
        ),
        model_file::model_compile,
    );
    r.register(
        ToolSpec::new(
            "model_open",
            "Open a .vmdl of the active addon in the running ModelDoc. Refuses while the live document is dirty unless force.",
            Mutating,
            &["modeldoc.open", "modeldoc.active"],
        ),
        model_live::model_open,
    );
    r.register(
        ToolSpec::new(
            "model_reload",
            "Reload the active ModelDoc document from disk (after model_edit); discards editor undo history. Refuses while dirty unless force.",
            Mutating,
            &["modeldoc.reload", "modeldoc.active"],
        )
        .destructive(true),
        model_live::model_reload,
    );
    r.register(
        ToolSpec::new(
            "model_save",
            "Save the active ModelDoc document to its own path (never Save As: refuses untitled documents and files outside the addon).",
            Writer,
            &["modeldoc.save", "modeldoc.active"],
        ),
        model_live::model_save,
    );
    r.register(
        ToolSpec::new(
            "model_node_types",
            "Every ModelDoc node class name (_class) the running editor knows, sorted; writeSnapshot stores them under hammer-mcp/research/.",
            ReadOnly,
            &["modeldoc.nodeTypes"],
        ),
        model_live::model_node_types,
    );
    // After the first-pass tools, before the vision render (which stays last).
    model_import::register(r);
    // The vision render (`modeldoc.render`, m2.14), appended last; the spec and handler live in
    // model_render.rs like particle_render's do.
    model_render::register(r);
}

/// The ModelDoc tools' order after [`super::hammer::POC_TOOL_ORDER`] (contract tests pin it);
/// `model_render` is appended last (the vision pass, WP-T).
pub const MODELDOC_TOOL_ORDER: [&str; 12] = [
    "model_status",
    "model_read",
    "model_list_nodes",
    "model_edit",
    "model_create",
    "model_compile",
    "model_open",
    "model_reload",
    "model_save",
    "model_node_types",
    "model_import",
    "model_render",
];
