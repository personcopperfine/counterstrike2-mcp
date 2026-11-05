//! Hammer tool set. `hammer.rs` registers the 24 POC tools (same names and order as
//! `../idabins/worknotes/mcpserver/server.py`), all implemented (no documented stubs remain;
//! workflows/03-hammer-tools.md). `modeldoc.rs` appends the ModelDoc
//! tools (`model_file.rs` file route, `model_live.rs` through the bridge). `particles.rs` appends
//! the particle tools (`particle_file.rs` file route; `particle_live.rs`, `particle_live_edit.rs`,
//! `particle_render.rs` through the bridge `pet.*`).
//! Entities: `entities_file.rs` (SAVED .vmap reads), `entities_live.rs` (create/set/undo),
//! `entities_query.rs` (live reads), `entity_classes.rs` (FGD), `lifecycle.rs` (save/reload/open/delete).
//! Mesh file reads: `mesh_file.rs` (SAVED .vmap; `src/vmap/mesh`).
//! Meshes: `mesh_live.rs` (create/primitive/setGeometry/setMaterial and the transform/move/extrude/delete
//! operators) over `crate::geometry`
//! (the port of the POC's `geometry.py`).
//! Props: `props.rs` (order), `model_catalog.rs`, `model_info.rs`, `props_query.rs`.
//! Triggers: `triggers.rs` (order), `triggers_query.rs`, `triggers_live.rs`, `trigger_volume.rs`,
//! with the volume/material argument parsing they share in `trigger_args.rs` (review D1).
//! Sounds: `sounds.rs` (order), `soundevents_query.rs`, `soundevents_edit.rs`, `sound_entities.rs`,
//! `sound_runtime.rs`, and the sound-key check `sound_check.rs` (library: `crate::soundevents`).
//! `bridge_job.rs` + `jobs.rs`: the bridge's job model (`job.*`) as a helper and the `compile_asset`/`job_status`/`job_cancel` tools;
//! `compile_map.rs` runs the headless map compile as a background job (bridge job, podman fallback); `run_map.rs` loads the compiled map in the engine over the VToolMessage `command` channel.
//! Selection: `selection_live.rs` (`get_selection` / `set_selection` over the bridge `selection.*`).
//! `live_common.rs` is the plumbing every live mutator shares (document read, `expectDoc`, the
//! unsaved-edit counter, the maybe-executed error taxonomy); `compile.rs` is the resourcecompiler
//! route both file routes share; `compile_map.rs` is the headless map compile (background job).
pub mod addon;
pub mod asset_resolve;
pub mod assets;
pub mod bridge_job;
pub mod compile;
pub mod compile_map;
pub mod console_live;
pub mod cs_script;
pub mod player_model;
pub mod entities_file;
pub mod entities_live;
pub mod entities_query;
pub mod entities_read;
pub mod entity_classes;
pub mod entity_io;
pub mod game_live;
pub mod hammer;
pub mod jobs;
pub mod layout;
pub mod lifecycle;
pub mod live_common;
pub mod mesh_file;
pub mod mesh_live;
pub mod mesh_read;
pub mod model_catalog;
pub mod model_facing;
pub mod model_file;
pub mod hammer_mesh;
pub mod model_import;
pub mod model_info;
pub mod model_live;
pub mod model_render;
pub mod new_map;
pub mod new_map_assets;
pub mod node_transform;
pub mod modeldoc;
pub mod particle_file;
pub mod particle_live;
pub mod particle_live_edit;
pub mod particle_render;
pub mod particles;
pub mod prefab;
pub mod play_test;
pub mod team;
pub mod props;
pub mod props_live;
pub mod props_query;
pub mod look;
pub mod render;
pub mod report_ready;
pub mod run_map;
pub mod scene_info;
pub mod selection_live;
pub mod session;
pub mod sound_check;
pub mod sound_entities;
pub mod sound_runtime;
pub mod sounds;
pub mod soundevents_edit;
pub mod soundevents_query;
pub mod trigger_args;
pub mod trigger_volume;
pub mod triggers;
pub mod triggers_live;
pub mod triggers_query;

/// The live mutators the unsaved-edit counter counts (`live_common::note_edit`): each successful
/// call suggests `save_map`, because nothing is ever auto-saved. `undo_last` is not here (it
/// takes an edit back), and neither are the file writers (they persist themselves).
pub const LIVE_MUTATORS: &[&str] = &[
    "create_entities",
    "set_entity_keyvalues",
    "delete_nodes",
    "create_mesh",
    "create_block",
    "apply_layout",
    "set_mesh_geometry",
    "set_mesh_material",
    "align_texture",
    "transform_mesh",
    "move_mesh_vertices",
    "extrude_mesh_face",
    "delete_mesh_faces",
    "create_props",
    "create_sound_entities",
    "create_trigger",
    "tie_to_entity",
    "move_to_world",
    "set_trigger_volume",
    "set_node_transform",
    "prefab",
    "add_entity_connections",
    "remove_entity_connections",
];

/// Mutating tools that do NOT edit the Hammer document (no `note_edit`): session/document
/// lifecycle, selection, the running game, and the ModelDoc / particle editors. The registry test
/// `mutating_tools_are_live_mutators_or_listed_here` keeps this and [`LIVE_MUTATORS`] a partition of
/// every `Mutating` tool.
pub const NOT_DOCUMENT_EDITS: &[&str] = &[
    "undo_last",
    "set_addon",
    "open_map",
    "new_map",
    "reload_map",
    "set_selection",
    "run_map",
    "console_exec",
    "input_set",
    "input_clear",
    "input_sequence",
    "play_test",
    "drain_sounds",
    "model_open",
    "model_reload",
    "particle_load",
    "particle_open",
    "particle_reload",
    "particle_live_edit",
    "particle_undo",
    "particle_redo",
    "particle_types",
];

/// Wire the `next` steps that belong to the iterate loop (edit -> save_map -> compile_map ->
/// run_map) and make the registry check that every step names a registered tool. Must run after
/// every tool is registered. `save_map`, `compile_map` and `run_map` set their own `next` (it
/// depends on the outcome); a live mutator's step is decided per call by [`mutator_next`].
pub fn wire_next_steps(r: &mut cs2_mcp_common::tools::Registry<crate::state::HammerState>) {
    r.after_call(|state, tool, out| {
        if LIVE_MUTATORS.contains(&tool) {
            mutator_next(state, out);
        }
        ready_next(tool, out);
    });
    r.require_known_steps();
}

/// After a successful `compile_map` (its poll names `run_map`) or `run_map` (it names `game_state`):
/// `report_ready` is the ship check that says whether the map is done.
fn ready_next(tool: &str, out: &mut cs2_mcp_common::tools::ToolOutput) {
    let (names, path_key) = match tool {
        "compile_map" => ("run_map", "path"),
        "run_map" => ("game_state", "contentPath"),
        _ => return,
    };
    let Some(steps) = out.next.as_mut() else { return };
    if !steps.iter().any(|s| s.tool == names) || steps.iter().any(|s| s.tool == "report_ready") {
        return;
    }
    let mut step = cs2_mcp_common::tools::NextStep::new(
        "report_ready",
        "ship check: saved, compiled, materials, spawns, mover lanes, objectives (done = it passes)",
    );
    if let Some(path) = out.json.get(path_key).and_then(|v| v.as_str()) {
        step = step.args(serde_json::json!({"map": path}));
    }
    steps.push(step);
}

/// After a successful live mutator: `save_map` when there is something unsaved to save. Not when
/// the call changed nothing (`undoGroups: 0`, e.g. a `tie_to_entity` whose meshes were already
/// tied), when nothing is unsaved, or when the document is untitled (`save_map` always refuses a
/// new document: say so in `hint` instead).
fn mutator_next(state: &crate::state::HammerState, out: &mut cs2_mcp_common::tools::ToolOutput) {
    if out.next.is_some() {
        return;
    }
    let changed_nothing = out.json.get("undoGroups").and_then(|v| v.as_u64()) == Some(0);
    if changed_nothing || state.unsaved_edits() == 0 {
        out.next = Some(Vec::new());
    } else if state.document_untitled() {
        out.next = Some(Vec::new());
        if let Some(m) = out.json.as_object_mut() {
            m.entry("hint").or_insert(serde_json::json!(
                "the document is untitled: save_map refuses it; save it once from Hammer's File menu \
                 (or work in a saved map) before save_map can persist these edits"
            ));
        }
    } else {
        out.next = Some(vec![cs2_mcp_common::tools::NextStep::new(
            "save_map",
            "persist the live edits to the .vmap (nothing is auto-saved)",
        )]);
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use cs2_mcp_common::tools::Mutability;
    use std::collections::BTreeSet;

    #[test]
    fn mutating_tools_are_live_mutators_or_listed_here() {
        let r = crate::build_registry();
        let mutating: BTreeSet<&str> = r
            .names()
            .into_iter()
            .filter(|n| !r.is_alias(n))
            .filter(|n| r.spec(n).is_some_and(|s| s.mutability == Mutability::Mutating))
            .collect();
        let declared: BTreeSet<&str> =
            LIVE_MUTATORS.iter().chain(NOT_DOCUMENT_EDITS).copied().collect();
        assert_eq!(
            mutating.difference(&declared).collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "Mutating tools in neither LIVE_MUTATORS nor NOT_DOCUMENT_EDITS: decide whether they count as unsaved edits"
        );
        assert_eq!(
            declared.difference(&mutating).collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "listed tools that are not registered as Mutating"
        );
        assert!(
            LIVE_MUTATORS.iter().all(|n| !NOT_DOCUMENT_EDITS.contains(n)),
            "the two lists are disjoint"
        );
    }

    #[test]
    fn a_successful_compile_or_run_suggests_report_ready_once() {
        use cs2_mcp_common::tools::ToolOutput;
        let poll = || {
            ToolOutput::from(serde_json::json!({"path": "maps/x.vmap"}))
                .with_next_args("run_map", "play it", serde_json::json!({"path": "maps/x.vmap"}))
        };
        let mut out = poll();
        ready_next("compile_map", &mut out);
        ready_next("compile_map", &mut out);
        let steps = out.next.unwrap();
        assert_eq!(steps.iter().map(|s| s.tool.as_str()).collect::<Vec<_>>(), ["run_map", "report_ready"]);
        assert_eq!(steps[1].args, Some(serde_json::json!({"map": "maps/x.vmap"})));
        // a compile start (next names compile_map), a failure (no next) and other tools get nothing
        let mut start = ToolOutput::from(serde_json::json!({})).with_next("compile_map", "wait");
        ready_next("compile_map", &mut start);
        assert_eq!(start.next.unwrap().len(), 1);
        let mut failed = ToolOutput::from(serde_json::json!({}));
        ready_next("run_map", &mut failed);
        assert!(failed.next.is_none());
        let mut other = poll();
        ready_next("job_status", &mut other);
        assert_eq!(other.next.unwrap().len(), 1);
    }

    #[test]
    fn every_wired_step_names_a_registered_tool() {
        let r = crate::build_registry();
        for n in r.names() {
            for step in &r.spec(n).unwrap().next_on_success {
                assert!(r.knows(&step.tool), "{n}: next names {}", step.tool);
            }
        }
        // the steps the handlers themselves emit (grep `with_next`)
        for t in ["save_map", "compile_map", "run_map", "game_state", "render_world", "render_view", "report_ready"] {
            assert!(r.knows(t), "{t}");
        }
    }
}
