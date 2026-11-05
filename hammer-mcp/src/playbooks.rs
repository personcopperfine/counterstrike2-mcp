//! Playbooks (streamline P4): step-by-step procedures a weaker agent follows through the tools
//! alone. Offered as MCP prompts (`prompts/list`, `prompts/get`) and, because many agents only
//! call tools, through the read-only `get_playbook` tool. The text lives here and nowhere else;
//! `tests/playbooks.rs` checks every tool name and argument key in it against the registry.
//!
//! Source: what ti_convoy actually did (workflows/streamline/ti_convoy-FIXME.md) and the
//! acceptance runs (ACCEPTANCE.md). `<ARG>` markers are replaced by the prompt arguments.
use crate::state::HammerState;
use cs2_mcp_common::prompts::{PromptArg, PromptSet, PromptSpec};
use cs2_mcp_common::tools::{CallCtx, Mutability::ReadOnly, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A playbook text must stay under this many characters (guarded by a test).
pub const MAX_PLAYBOOK_CHARS: usize = 2_500;

const BLOCKOUT_MAP: &str = r#"Playbook blockout_map: a greybox level from nothing to compiled, loaded and walked. Map: <MAP>.
1. session_status, read next. set_addon {addon} if the addon is wrong.
2. new_map {path:<NEWPATH>, clear:"gameplay"} (the name the user gave): the template's meshes, spawns, zones are gone; sky, light and fog stay.
3. Plan: floor, walls, cover, 2+ spawns per team, movers. get_entity_class {class} for an unknown class.
4. apply_layout {layout:{meshes:[{kind:"block", id:"floor", min:[0,0,-16], max:[2048,512,0]}], entities:[{id:"ct1", class:"info_player_counterterrorist", origin:[64,0,1]}, {id:"listener", class:"logic_eventlistener", origin:[0,0,128], keyValues:{targetname:"listener", EventName:"round_freeze_end"}}]}}. Max 32 meshes, 64 entities per call; dryRun:true checks first; the result maps ids to nodeIDs. Keep spawns out of cover and of any mover's sweep.
5. A mover: tie_to_entity {meshNodeIDs:[...], class:"func_movelinear", targetname:"mover", keyValues:{movedir:"0 0 0", movedistance:1500, speed:200}}. Start it when play begins: add_entity_connections {nodeID, connections:[{output:"OnEventFired", target:"mover", input:"Open"}]} on the listener's nodeID. logic_auto OnMapSpawn fires at load, before the round.
6. save_map (saved:true). Then look {target:"map", mode:"tour", source:"editor"} (it frames the saved bounds). Describe every image in writing (present, missing, overlapping); fix with set_node_transform or delete_nodes, save_map, look again.
7. compile_map {<PATHKV>mode:"novis", waitMs:30000}: starts and waits; repeat compile_map {jobId, waitMs:30000} while done is false. Success: state ok, failed 0. EngineBusy: the engine holds the map, run_map {unload:true} first.
8. run_map {<PATHKV>team:"ct"} (default: dead on team 0). play_test: an input step walks the lane (waits out freeze time), then wait_entity or wait_entity {name:"mover", path:"origin[0]", gt:300} proves the mover moved.
9. look {mode:"player_eye"}: the COMPILED map. Describe it.
10. report_ready {<MAPKV>mode:"none"} (mode defuse or hostage only if the user asked for one). Fix each fail it names; every change repeats steps 6-10 (unload first).
Done = compile ok with 0 failed, report_ready without a fail and each warn or skipped explained, the mover or goal proven live, and your written description of the looks. If you had to step outside these tools, say so and stop; add it to the project's FIXME file if there is one."#;

const ITERATE_LOOP: &str = r#"Playbook iterate_loop: change a map, compile, load, test, look; repeat until the check passes. Map: <MAP>.
1. session_status: the document, unsaved edits, jobs. Nothing else runs while a compile job is running.
2. Edit the live document: apply_layout, set_node_transform, set_mesh_geometry, set_entity_keyvalues, create_entities, delete_nodes; undo_last reverts one group. Prefer one apply_layout over many calls.
3. look {target:"mover", mode:"angles", source:"editor"} (the live document; target = what you changed: a targetname, nodeID or box). Describe what you see before judging.
4. save_map. compile_map refuses UnsavedChanges until you do.
5. compile_map {<PATHKV>mode:"novis", waitMs:30000} (the iteration compile; stages:["world","phys"] is the smallest useful set; mode "full" for the last build). Repeat compile_map {jobId, waitMs:30000} while done is false. Never busy-poll. state must be ok; failLines and logTail say why not. EngineBusy: run_map {unload:true} first (the engine holds the old map).
6. run_map {<PATHKV>team:"ct"} loads the COMPILED map; unsaved edits are not in it. A stale vpk is staleCompile.
7. Test, in this order of evidence: play_test with wait_entity / expect_entity (a RUNNING-game entity), wait_log {pattern} for a script line, game_rules for freeze and round state, entity_find for positions. Round-start logic uses a logic_eventlistener on round_freeze_end, not logic_auto. Input quirks: release and tap of one key in one call re-presses it; while input_set angles is set, console setang is ignored (input_clear).
8. look {target:"mover", mode:"frames", count:4} or {mode:"player_eye"} in the engine (frames needs a target or a pose). If it differs from the editor look, the compile is stale: compile again.
9. Fix what the evidence shows, back to step 2. After the last change run a full compile (mode "full"), run_map, play_test once more.
10. report_ready {<MAPKV>mode:"none"} (mode defuse or hostage if the map is one). Fix each fail it names.
Done = last compile ok with 0 failed, report_ready without a fail and each warn or skipped explained in your answer, the behaviour you changed proven by play_test or entity_find, and a written description of the last look. If you had to step outside these tools, say so and stop; log it in the project's FIXME file if there is one."#;

const CS_SCRIPT: &str = r#"Playbook cs_script: write a map script (CS2 point_script, TypeScript), build it, attach it, test it. Script: <SCRIPT>. Map: <MAP>.
1. session_status, set_addon if needed. cs_script {action:"status"}: no project -> cs_script {action:"init", scripts:["<SCRIPT>"]} (npm install; types.verdict different: the scripts type-check against the game's own point_script.d.ts, keep that). Project lacking <SCRIPT> -> cs_script {action:"add", name:"<SCRIPT>"}.
2. cs_script {action:"write", name:"<SCRIPT>", file:"index.ts", content:"..."}: the whole file; action "read" shows it. Author only the .ts. Log at the top level, e.g. Instance.Msg("[<SCRIPT>] loaded"): it proves the load.
3. cs_script {action:"build", name:"<SCRIPT>"}: ok true. Else failed[].errors (file, line, col, code, message): fix with write, build again. reload.reloaded true = a running map took it. Then cs_script {action:"compile", name:"<SCRIPT>"} before any run_map: the map loads the .vjs_c, never the .js (an error naming bridge m3.17: ask the user to reinstall it).
4. find_entities {class:"point_script"}. Missing: create_entities {entities:[{class:"point_script", origin:[0,0,0], keyValues:{targetname:"script", cs_script:"maps/scripts/<SCRIPT>.vjs"}}]}, then save_map.
5. compile_map {<PATHKV>mode:"novis", waitMs:30000}, then compile_map {jobId, waitMs:30000} while done is false; state ok. EngineBusy: run_map {unload:true} first. run_map {<PATHKV>team:"ct"}.
6. play_test {steps:[{kind:"wait_log", pattern:"[<SCRIPT>] loaded", timeoutMs:20000}]}; then wait_entity / expect_entity on what the script moves or spawns; console_exec {command:"<test command>"} for its test commands (cheats: sv_cheats is on). look {target:"mover", mode:"frames", count:4}: describe them.
7. After the last build: cs_script {action:"compile", name:"<SCRIPT>"} again. report_ready {<MAPKV>profile:"release"}: loose_compiled lists the .vjs_c (it ships beside the vpk); debug_patterns names debug flags and RegisterCheatCommand: remove them or explain each.
Done = build ok, map compile ok with 0 failed, the script's log line and effect seen live, compile ok, report_ready release without a fail, each warn and skipped check explained (no_metamod_load is always skipped: the user must load the map once without Metamod; the verdict stays incomplete for that reason alone), a written look. If you stepped outside these tools, say so and log it in the project's FIXME file."#;

const PARTICLE_SET: &str = r#"Playbook particle_set: author, compile and see a set of particle effects. Folder: <FOLDER>. Map: <MAP>.
1. session_status; particle_status.
2. Per effect: particle_create {path:"<FOLDER>/fx1.vpcf"}, then particle_edit {path:"<FOLDER>/fx1.vpcf", ops:[{op:"set_root_field", key:"m_nMaxParticles", value:64}]}; particle_schema and particle_types list valid classes and fields; edits are all-or-nothing. particle_validate {path} must return ok:true with no errors.
3. A child effect is an op inside particle_edit: particle_edit {path, ops:[{op:"add_child", child:{m_ChildRef:{"$flag":"resource", value:"<FOLDER>/child.vpcf"}}}]}; create the child file too.
4. particle_compile {path:"<FOLDER>", recursive:true}: ok only if every file compiled. Children compile first. The output is loose files under game/csgo_addons/<addon>/, NOT inside the map vpk.
5. See each one: particle_load (once), particle_open {path} (switches the effect; do it for every file), particle_render {name:"fx1"} (a file stem). After a later particle_edit of the open file: particle_reload (no path: it reloads the open document, validating first). Describe each image (shape, colour, motion, density); tune and render again.
6. In the map: create_entities {entities:[{class:"info_particle_system", origin:[0,0,64], keyValues:{targetname:"fx", effect_name:"<FOLDER>/fx1.vpcf", start_active:1}}]}; save_map; compile_map {<PATHKV>mode:"novis", waitMs:30000} (EngineBusy: run_map {unload:true} first), then compile_map {jobId, waitMs:30000} while done is false; run_map {<PATHKV>team:"ct"}; look {target:"fx", mode:"frames", count:4, intervalMs:500}. Describe the frames.
7. report_ready {<MAPKV>profile:"release"}: loose_compiled warns for each compiled particle the map names that is outside the vpk. The Workshop upload must carry the addon folder: tell the user.
Done = particle_validate ok and particle_compile ok for every file, the effect seen in the engine (written description), compile_map ok with 0 failed, report_ready release without a fail and each warn and each skipped check explained (no_metamod_load is always skipped: the user must load the map once without Metamod; the verdict stays incomplete for that reason alone). Stop and Start of one system in one command line drops the Start. If you stepped outside these tools, say so and log it in the project's FIXME file."#;

const RELEASE: &str = r#"Playbook release: the last pass before a map ships. Map: <MAP>.
1. session_status. Unsaved edits: save_map. A loaded map blocks compiling: run_map {unload:true}.
2. compile_map {<PATHKV>mode:"full", waitMs:30000} (not novis: a release needs VIS and the nav); repeat compile_map {jobId, waitMs:30000} while done is false. state must be ok and failed 0; otherwise read failLines, fix, save, repeat.
3. report_ready {<MAPKV>profile:"release"} (add mode defuse or hostage if the map is one). Read every check, not only the verdict:
 - saved / compiled: save or compile again.
 - materials: a missing or texture-less material renders magenta; replace it with set_mesh_material.
 - spawns, spawn_in_mover_path, objectives: move the spawn; explain objectives if the mode needs none.
 - loose_compiled: compiled files beside the vpk (particles, scripts); the upload must carry the addon folder, say so.
 - debug_patterns: debug flags, RegisterCheatCommand, sv_cheats; remove them or explain each.
 - test_cvars: needs the game running (step 4 first, then step 5 reads the real value).
 - no_metamod_load is always skipped: the tools session loads Metamod and this tool cannot test without it. It is the expected skip: tell the user to load the map once without Metamod. The verdict stays incomplete for that reason alone.
4. run_map {<PATHKV>team:"ct"} and again with team "t". play_test the core behaviour of the mode (objective, mover, round start); game_rules shows freeze and round state.
5. report_ready {<MAPKV>profile:"release"} again, now that the game runs, so test_cvars is checked.
6. look {target:"map", mode:"tour", source:"engine"} and look {mode:"player_eye"} from each team's spawn. Describe every image in writing: missing floors, magenta, floating cover, dark rooms.
7. After any fix repeat steps 1-5 once; do not loop on the verdict.
Done = full compile ok with 0 failed, report_ready release has no fail and every warn and every skipped check (no_metamod_load first) is explained in your answer with its manual step, both teams spawned alive and played, and your written description of the looks. If you had to step outside these tools, say so and stop; log it in the project's FIXME file if there is one."#;


const IMPORT_KINDS: [&str; 4] = ["prop", "world", "hammer_mesh", "player"];

const IMPORT_HEAD: &str = r#"Playbook import_model: a model file into a map, compiled and seen in the engine. Source: <SOURCE>. Kind: <KIND>.
1. session_status; set_addon if the addon is wrong. A CS2 model's front is +X: a glTF is turned for you, an FBX, OBJ or SMD is not, so check its facing in step 3.
2. model_import {source:"<SOURCE>", kind:"<KIND>", dryRun:true}: read the plan, warnings, dropped and materialNames. Then the same call without dryRun. It must come back ok with the compile ok. CompileFailed: read failLines, fix the source, call again with overwrite:true. materials:"existing" + materialMap reuses game materials. <result.model> below = the result's model.
"#;

const IMPORT_PROP: &str = r#"3. get_model_info {model:"<result.model>"}: bounds, facing (unknown is normal), sequences. To see it: model_open the .vmdl, model_render. Wrong facing: turn the source, model_import with overwrite:true.
4. create_props {props:[{kind:"<PROPKIND>", model:"<result.model>", origin:[0,0,0]}]}; a rigged result (animations listed) plays with keyValues:{StartingAnim:"<clip>"} on a dynamic prop. Optional: prefab {action:"create", name:"<set>", nodeIDs:[...], centered:true} turns nodes into maps/prefabs/<set>.vmap and REPLACES them with a prefab node; prefab {action:"instance", name:"<set>", origin:[512,0,0]} adds copies.
5. save_map. compile_map {mode:"novis", waitMs:30000}; repeat compile_map {jobId, waitMs:30000} while done is false; state ok, failed 0. EngineBusy: run_map {unload:true} first. run_map {team:"ct"}.
6. look {target:<nodeID>, mode:"angles"} (a saved prefab nodeID works; the box is 64 units around the origin: a big model needs a pose from the bounds). An animated model: look {mode:"frames", pose:{origin:[...], angles:[...]}, count:4}. Describe: present, facing, materials, scale.
"#;

const IMPORT_MESH: &str = r#"3. The result lists nodeIDs, chunks and warnings. ChunkFailed: delete_nodes {nodeIDs:[...]} for the created ones, fix, call again. look {target:<first nodeID>, source:"editor"}: describe the faces, textures and anything missing. get_model_info does not apply (these are meshes, not a model).
4. Optional: prefab {action:"create", name:"<set>", nodeIDs:[...], centered:true} makes them reusable (it REPLACES the nodes with a prefab node); prefab {action:"instance", name:"<set>", origin:[512,0,0]} places a copy.
5. save_map. compile_map {mode:"novis", waitMs:30000}; repeat compile_map {jobId, waitMs:30000} while done is false; state ok, failed 0. EngineBusy: run_map {unload:true} first. run_map {team:"ct"}.
6. look {target:<nodeID>, mode:"angles"} on the compiled map. Describe it.
"#;

const IMPORT_PLAYER: &str = r#"3. The rig is retargeted onto the CS2 player skeleton; read the mapping, markup (weapon attachments + hitboxes copied from referenceAgent; markup.dropped must be empty) and dropped. Check it first: create_props {props:[{kind:"dynamic", model:"<result.model>", origin:[0,0,0]}]}, then look {target:<nodeID>, source:"editor"}.
4. cs_script {action:"status"}; no project: cs_script {action:"init"}. player_model {action:"set", model:"<result.model>"} (team:"ct" or "t" for one side) writes the script, a point_script and a hidden precache prop (without it: ERROR model). player_model {action:"status"} must say inSync.
5. save_map. compile_map {mode:"novis", waitMs:30000}; repeat compile_map {jobId, waitMs:30000} while done is false. run_map does NOT show it (models load on demand, the pawn stays at the origin). Verify in another server, client-mcp: launch_session {kind:"wine-tools", addon:<addon>, map:<map>}, console_exec {command:"sv_cheats 1; thirdperson"}, screenshot. Describe the pawn and where its weapon is (in hand?).
"#;

const IMPORT_TAIL: &str = r#"The running engine keeps a model it already loaded: after re-importing or recompiling one, restart the tools session and recompile the map (it embeds models) before judging a look.
report_ready {<MAPKV>mode:"none"}: fix each fail it names.
Done = model_import ok with the compile ok, the model seen in the engine (written description: facing, materials, scale; player: the pawn), compile ok with 0 failed, report_ready without a fail and each warn or skipped explained. If you had to step outside these tools, say so and stop; log it in the project's FIXME file if there is one."#;

/// `(name, one-line description, arguments, text)`.
fn table() -> [(&'static str, &'static str, Vec<PromptArg>, &'static str); 6] {
    let map = |what: &'static str| PromptArg { name: "map", description: what, required: false };
    [
        (
            "blockout_map",
            "A greybox map from an empty addon to compiled, loaded and walked",
            vec![map("maps/<name>.vmap to create (default: ask the user for the name)")],
            BLOCKOUT_MAP,
        ),
        (
            "iterate_loop",
            "Edit, save, compile (novis), run_map, play_test, look; repeat",
            vec![map("maps/<name>.vmap (default: the active document)")],
            ITERATE_LOOP,
        ),
        (
            "cs_script",
            "Write, build, attach and test a point_script (TypeScript project, build, compile, play_test)",
            vec![PromptArg {
                name: "script",
                description: "script name under maps/scripts (default: ask the user)",
                required: false,
            }],
            CS_SCRIPT,
        ),
        (
            "particle_set",
            "Author, validate, compile and see a folder of .vpcf effects",
            vec![PromptArg {
                name: "folder",
                description: "addon-relative particles/<set> folder (default: ask the user)",
                required: false,
            }],
            PARTICLE_SET,
        ),
        (
            "release",
            "Full compile and the release report_ready before a map ships",
            vec![map("maps/<name>.vmap (default: the active document)")],
            RELEASE,
        ),
        (
            "import_model",
            "Import a glTF/FBX/OBJ/SMD as a prop, world model, Hammer meshes or a player skin; compile, place, see it",
            vec![
                PromptArg { name: "source", description: "model file (host path or inside the addon; default: ask the user)", required: false },
                PromptArg { name: "kind", description: "prop (default) | world | hammer_mesh | player", required: false },
            ],
            IMPORT_HEAD,
        ),
    ]
}

/// The playbook `name` with its arguments filled in; `None` for an unknown name. Without a `map`
/// argument no concrete path is ever written: the calls omit `path` / `map` (they default to the
/// active document) and the text says "your map (the active document)".
pub fn playbook_text(name: &str, args: &BTreeMap<String, String>) -> Option<String> {
    let (_, _, _, text) = table().into_iter().find(|(n, ..)| *n == name)?;
    let text: String = if name == "import_model" {
        let kind = args
            .get("kind")
            .map(|k| k.trim())
            .filter(|k| IMPORT_KINDS.contains(k))
            .unwrap_or("prop");
        let body = match kind {
            "hammer_mesh" => IMPORT_MESH,
            "player" => IMPORT_PLAYER,
            _ => IMPORT_PROP,
        };
        let note = if args.get("kind").is_some_and(|k| IMPORT_KINDS.contains(&k.trim())) {
            ""
        } else {
            "Kinds: prop (default), world (static level geometry), hammer_mesh (Hammer meshes), player (a CS2 player skin): pass kind for their steps.\n"
        };
        let propkind = if kind == "world" { "static" } else { "dynamic" };
        format!("{text}{note}{body}{IMPORT_TAIL}").replace("<PROPKIND>", propkind).replace("<KIND>", kind)
    } else {
        text.to_string()
    };
    let get = |key: &str| args.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
    let map = get("map");
    let kv = |key: &str| map.map(|m| format!("{key}:\"{m}\", ")).unwrap_or_default();
    Some(
        text.replace("<SOURCE>", get("source").unwrap_or("<file>"))
            .replace("<MAP>", map.unwrap_or("your map (the active document)"))
            .replace("<NEWPATH>", &map.map(|m| format!("\"{m}\"")).unwrap_or_else(|| "\"maps/<name>.vmap\"".into()))
            .replace("<PATHKV>", &kv("path"))
            .replace("<MAPKV>", &kv("map"))
            .replace("<SCRIPT>", get("script").unwrap_or("<script>"))
            .replace("<FOLDER>", get("folder").unwrap_or("particles/<set>").trim_end_matches('/')),
    )
}

/// `(name, description, argument names)` of every playbook, in order.
pub fn playbook_index() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    table()
        .into_iter()
        .map(|(n, d, args, _)| (n, d, args.iter().map(|a| a.name).collect()))
        .collect()
}

/// The playbooks as MCP prompts.
pub fn prompt_set() -> PromptSet {
    let mut set = PromptSet::new();
    for (name, description, arguments, _) in table() {
        set.register(PromptSpec {
            name,
            description,
            arguments,
            render: Arc::new(move |args| playbook_text(name, args).unwrap_or_default()),
        });
    }
    set
}

/// `get_playbook {name?, map?, script?, folder?}`: the same texts `prompts/get` renders, for agents
/// that only call tools. No name lists the playbooks.
pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "get_playbook",
            "Step-by-step procedure for a whole task, as text (the same as the MCP prompt): blockout_map, iterate_loop, cs_script, particle_set, release, import_model. No name lists them. Pass map, script, folder, source or kind to fill in the calls (else they use the active document). Read the matching one before such a task.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "playbook; omit to list"},
                "source": {"type": "string", "description": "model file (import_model)"},
                "kind": {"type": "string", "description": "prop|world|hammer_mesh|player (import_model)"},
                "map": {"type": "string", "description": "maps/x.vmap (blockout_map, iterate_loop, release)"},
                "script": {"type": "string", "description": "script file stem (cs_script)"},
                "folder": {"type": "string", "description": "particles/<set> (particle_set)"}
            },
            "additionalProperties": false
        })),
        get_playbook,
    );
}

async fn get_playbook(_state: HammerState, _ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let name = match args.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!("name must be a playbook name, got {other}")))
        }
    };
    let Some(name) = name else {
        let list: Vec<Value> = playbook_index()
            .into_iter()
            .map(|(name, summary, arguments)| json!({"name": name, "summary": summary, "arguments": arguments}))
            .collect();
        return Ok(ToolOutput::from(json!({"playbooks": list})).with_next(
            "get_playbook",
            "read the one that matches the task (pass name, and map/script/folder if you have them)",
        ));
    };
    let mut values = BTreeMap::new();
    for key in ["map", "script", "folder", "source", "kind"] {
        if let Some(v) = args.get(key).and_then(Value::as_str) {
            values.insert(key.to_string(), v.to_string());
        }
    }
    if let Some(kind) = values.get("kind").filter(|k| !IMPORT_KINDS.contains(&k.trim())) {
        return Err(ToolError::invalid_arguments(format!("kind must be one of {IMPORT_KINDS:?}, got {kind:?}")));
    }
    match playbook_text(&name, &values) {
        Some(text) => Ok(ToolOutput::from(json!({"name": name, "playbook": text})).no_next()),
        None => {
            let names: Vec<&str> = playbook_index().into_iter().map(|(n, ..)| n).collect();
            Err(ToolError::new("UnknownPlaybook", "UnknownPlaybook", format!("no playbook {name:?}"))
                .with_code(-32602)
                .retry_safe(true)
                .did_you_mean(cs2_mcp_common::tools::nearest_names(&name, names.iter().copied(), 3))
                .with_data(json!({"playbooks": names})))
        }
    }
}
