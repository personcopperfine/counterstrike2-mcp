//! The 24 POC tools, 1:1 (names, order, bridge methods, mutability). The bodies live in the
//! per-cluster modules registered at their POC positions below; no stubs remain.
use crate::state::HammerState;
use cs2_mcp_common::tools::{Mutability::*, Registry, ToolSpec};


pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "session_status",
            "Bridge/session status: build, liveness (mainThreadAgeMs), capabilities, active document + host path, unsaved bridge edits, jobs. Never raises.",
            ReadOnly,
            &[],
        ),
        super::session::session_status,
    );
    // POC order: every tool lives in its cluster module and is registered at its POC position.
    for name in POC_TOOL_ORDER.iter().skip(1) {
        match *name {
            "create_entities" => super::entities_live::register_create(r),
            "undo_last" => super::entities_live::register_undo(r),
            "save_map" => super::lifecycle::register_save(r),
            "reload_map" => super::lifecycle::register_reload(r),
            "run_map" => super::run_map::register(r),
            "open_map" => super::lifecycle::register_open(r),
            "list_entities" => super::entities_file::register_one(r, name),
            "find_entities" => super::entities_read::register_find(r),
            "get_entity" => super::entities_read::register_get(r),
            "list_meshes" | "get_mesh" => super::mesh_file::register_one(r, name),
            "create_mesh" => super::mesh_live::register_create(r),
            "create_block" => super::mesh_live::register_block(r),
            "set_mesh_geometry" => super::mesh_live::register_geometry(r),
            "transform_mesh" => super::mesh_live::register_transform(r),
            "move_mesh_vertices" => super::mesh_live::register_move_vertices(r),
            "extrude_mesh_face" => super::mesh_live::register_extrude(r),
            "delete_mesh_faces" => super::mesh_live::register_delete_faces(r),
            "set_mesh_material" => super::mesh_live::register_material(r),
            "delete_nodes" => super::lifecycle::register_delete(r),
            "render_view" | "render_world" => super::render::register_one(r, name),
            "compile_map" => super::compile_map::register(r),
            other => unreachable!("POC tool {other} has no module"),
        }
    }
    // entity extensions after the 24 POC tools ([`ENTITY_TOOL_ORDER`])
    super::entities_live::register_set(r);
    super::entity_classes::register(r);
    super::scene_info::register(r);
    super::entities_query::register_aliases(r);
}

/// Entity tools beyond the POC, registered right after [`POC_TOOL_ORDER`] (contract tests pin it).
pub const ENTITY_TOOL_ORDER: [&str; 6] = [
    "set_entity_keyvalues",
    "list_entity_classes",
    "get_entity_class",
    "get_scene_info",
    // deprecated aliases (listed last, one-line description each)
    "get_entity_live",
    "find_entities_live",
];

/// The POC's tool order (contract tests pin it).
pub const POC_TOOL_ORDER: [&str; 24] = [
    "session_status",
    "create_entities",
    "undo_last",
    "save_map",
    "reload_map",
    "run_map",
    "open_map",
    "list_entities",
    "find_entities",
    "get_entity",
    "render_view",
    "render_world",
    "list_meshes",
    "get_mesh",
    "create_mesh",
    "create_block",
    "set_mesh_geometry",
    "transform_mesh",
    "move_mesh_vertices",
    "extrude_mesh_face",
    "delete_mesh_faces",
    "set_mesh_material",
    "delete_nodes",
    "compile_map",
];
