//! `new_map {path, template?, open?, overwrite?}`: create a `.vmap` in the active addon and open
//! it. Nothing else creates one: `open_map` needs an existing file, and the edit tools need a
//! loaded Hammer, which only loads with a document open (the first map opens over the VToolMessage
//! `open_asset` post, `open_map`'s first-map route). In an empty addon this is the way in.
//!
//! * **Template.** The default is Valve's own `addon_template` map,
//!   `<app>/content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap` (app = the runtime config's
//!   `<project-root>/workshop/run/app`). Any other `.vmap` of the active addon can be named instead.
//!   The template is parsed with this crate's own reader first (a file the reader refuses is never
//!   copied: Hammer's `OpenFile` would pop its modal on it) and its contents are reported, so the
//!   caller knows what it starts with. The copy is byte for byte: it carries the template's
//!   thumbnail and asset references, the way Hammer's own "Save As" would. The default template
//!   references resources that exist only in the template ADDON (a `.vpost`, the soundscape events):
//!   `new_map_assets` copies what it can without replacing anything, and the result's
//!   `templateAssets {copied, missing, alreadyPresent}` lists every reference either way.
//! * **No `empty` template.** A map without the template's spawns, light and sky is cheap to write
//!   with `cs2-dmx`, but nothing here can prove Hammer loads it (the proof is a live open), so it is
//!   not offered (FIXME.md "new_map").
//! * **Writes** only inside `content/csgo_addons/<addon>/maps/` of the SELECTED addon (the same
//!   canonical `maps/....vmap` shape `open_map` takes, no symlink may lead out), refuses an existing
//!   target unless `overwrite:true` (the old file is then kept as `<name>.vmap.bak-<seconds>[-k]`),
//!   and writes atomically (temporary file + rename; without `overwrite` the temporary is
//!   hard-linked into place, which cannot replace a file created meanwhile). A path that differs
//!   from an existing file or directory only in case is refused (`MapExists`, `didYouMean`): this
//!   host is case-sensitive, Hammer under Wine is not. The template addon itself is never written.
//! * **Clears** (`clear: "gameplay"|"all"|"none"`, default none): after the open, deletes the
//!   template's nodes live through `node.delete` (the op `delete_nodes` uses; one request = one undo
//!   group per 256 ids). The ids come from the template's own parsed file, whose node ids are the
//!   live ones for a freshly opened copy. `gameplay` removes every mesh and the entities that
//!   shape play (spawns, buy/bomb/hostage zones, `func_*`/`trigger_*`/`prop_*`/`logic_*`/`game_*`/
//!   `point_*`, `info_target`, `weapon_*`/`item_*`) and KEEPS the environment (sky, light, fog,
//!   cubemap, post-processing, `info_map_parameters`, soundscapes, light probes, the minimap
//!   boundary) and the match-flow cameras (`team_select`, `end_of_match`, the team intros); `all`
//!   deletes every entity and mesh, keeping worldspawn (and any node of another kind: nav data,
//!   groups, prefabs, listed under `kept`). The result's `clear` lists what was planned, removed
//!   (the chunks that succeeded) and kept by class; it is `skipped` when the active document is not
//!   the one just opened.
//! * **Opens** through `open_map`'s own code (`lifecycle::open_map_locked`, same write gate), so the
//!   first-map vtm route, the engine-addon check and the document view are `open_map`'s.
use crate::state::HammerState;
use cs2_mcp_common::tools::{
    nearest_names, CallCtx, Mutability::Mutating, Registry, ToolError, ToolOutput, ToolSpec,
};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The new-map tool's order, after [`super::layout::LAYOUT_TOOL_ORDER`] (contract tests pin it).
pub const NEW_MAP_TOOL_ORDER: [&str; 1] = ["new_map"];

/// Valve's template addon, under `<app>/content/csgo_addons/`.
pub const TEMPLATE_ADDON: &str = "addon_template";
/// The template map's file name inside `<TEMPLATE_ADDON>/maps/`.
pub const TEMPLATE_MAP: &str = "xxx_mapname_xxx.vmap";

/// How many map names `session_status` lists.
pub const STATUS_MAP_CAP: usize = 20;
/// The deepest `maps/` sub-directory the listing walks, and the most files it looks at.
const WALK_DEPTH: usize = 4;
const WALK_FILES: usize = 5000;

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "new_map",
            "Create a .vmap in the active addon from a template and (default) open it. Use when the addon has no map yet (Hammer loads only with a document open). Not for: opening an existing file (open_map). \
             template: addon_template (default; a ready CS2 map with spawns, bomb sites, buy zones, sky, light) or another .vmap of the addon. \
             Refuses an existing target unless overwrite:true (old file kept as .bak-<seconds>). \
             clear:\"gameplay\" deletes meshes (toolsskybox too), spawns, buy/bomb zones, worldtext (keeps sky, light, fog); clear:\"all\" keeps only worldspawn; both need open. Not saved: save_map.",
            Mutating,
            // like open_map: the bridge may not advertise anything yet (hammer.dll loads with the
            // first document); `open_map_locked` decides the route itself
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "maps/<name>.vmap"
                },
                "template": {
                    "type": "string",
                    "description": "\"addon_template\" (default) or a .vmap of the addon, e.g. maps/base.vmap"
                },
                "open": {
                    "type": "boolean",
                    "description": "default true"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "default false: an existing file is refused"
                },
                "clear": {
                    "type": "string",
                    "enum": ["none", "gameplay", "all"],
                    "description": "delete the template's nodes after opening; default none; needs open"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })),
        new_map,
    );
}

/// The `.vmap` files of the selected addon as `maps/...` content paths, sorted, and how many there
/// are in all (only the first `cap` are returned). Case-insensitive on the extension, hidden
/// entries skipped, bounded in depth and in files looked at; a missing `maps/` is an empty list.
pub fn addon_maps(state: &HammerState, cap: usize) -> (Vec<String>, usize) {
    let root = state.content().addon_root();
    let mut found: Vec<String> = Vec::new();
    let mut seen = 0usize;
    let mut pending: Vec<(PathBuf, String, usize)> = vec![(root.join("maps"), "maps".into(), 0)];
    while let Some((dir, rel, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > WALK_FILES {
                break;
            }
            let Ok(name) = entry.file_name().into_string() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let child = format!("{rel}/{name}");
            if kind.is_dir() {
                if depth < WALK_DEPTH {
                    pending.push((entry.path(), child, depth + 1));
                }
            } else if kind.is_file() && name.to_ascii_lowercase().ends_with(".vmap") {
                found.push(child);
            }
        }
    }
    found.sort();
    let total = found.len();
    found.truncate(cap);
    (found, total)
}

/// The template map Valve ships: `<app>/content/csgo_addons/addon_template/maps/xxx_mapname_xxx.vmap`,
/// else (a game update renamed it) the first `.vmap` in that directory, by name.
pub fn default_template(state: &HammerState) -> Option<PathBuf> {
    let dir = state
        .app_root()
        .join("content/csgo_addons")
        .join(TEMPLATE_ADDON)
        .join("maps");
    let named = dir.join(TEMPLATE_MAP);
    if named.is_file() {
        return Some(named);
    }
    let mut maps: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vmap")))
        .collect();
    maps.sort();
    maps.into_iter().next()
}

/// What the template holds, from this crate's own reader: entity/mesh counts, per-class counts and
/// the spawn points a CS2 map needs.
fn contents_of(map: &crate::vmap::Document, bytes: usize) -> Value {
    let mut by_class: std::collections::BTreeMap<String, usize> = Default::default();
    for entity in map.entities() {
        let class = if entity.classname.is_empty() { "(none)" } else { entity.classname.as_str() };
        *by_class.entry(class.to_string()).or_default() += 1;
    }
    let count = |class: &str| by_class.get(class).copied().unwrap_or(0);
    json!({
        "entities": map.entities().len(),
        "meshes": map.meshes().len(),
        "spawns": {
            "info_player_counterterrorist": count("info_player_counterterrorist"),
            "info_player_terrorist": count("info_player_terrorist"),
            "info_player_start": count("info_player_start"),
        },
        "byClass": by_class,
        "bytes": bytes,
    })
}

fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => Err(ToolError::invalid_arguments(format!("{name}: a boolean, got {other}"))),
    }
}

/// The write target must stay inside `<addon>/maps/`: walk up from the target's parent to the first
/// directory that exists and require its real path (symlinks resolved) to be inside the real
/// `maps/` directory. The shape gate already refuses `..`, so this only catches a symlink.
fn inside_maps(maps_root: &Path, parent: &Path) -> Result<(), ToolError> {
    let real_root = resolve_existing_prefix(maps_root);
    let real = resolve_existing_prefix(parent);
    if real.starts_with(&real_root) {
        return Ok(());
    }
    Err(ToolError::new(
        "PathOutsideAddon",
        "PathOutsideAddon",
        format!(
            "{} resolves outside the addon's maps/ directory ({}); new_map writes only inside it",
            parent.display(),
            real_root.display()
        ),
    )
    .with_code(-32602)
    .retry_safe(true))
}

/// `path` with its deepest EXISTING ancestor canonicalized (symlinks resolved) and the components
/// that do not exist yet appended unchanged.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut probe = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(probe) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (probe.parent(), probe.file_name()) {
            (Some(up), Some(name)) => {
                tail.push(name);
                probe = up;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `content_path` with every EXISTING component spelled as it is on disk (a case-insensitive match
/// when the exact name is absent), the components that do not exist yet unchanged, and whether all
/// components exist. This host is case-sensitive, Hammer under Wine is not: `maps/Foo.vmap` and
/// `maps/foo.vmap` are one file to Hammer.
pub fn resolve_case(addon_root: &Path, content_path: &str) -> (String, bool) {
    let mut dir = addon_root.to_path_buf();
    let mut parts: Vec<String> = Vec::new();
    let mut complete = true;
    for component in content_path.split('/') {
        if !complete {
            parts.push(component.to_string());
            continue;
        }
        let exact = dir.join(component);
        let real = if std::fs::symlink_metadata(&exact).is_ok() {
            Some(component.to_string())
        } else {
            std::fs::read_dir(&dir).ok().and_then(|entries| {
                let mut names: Vec<String> = entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.eq_ignore_ascii_case(component))
                    .collect();
                names.sort();
                names.into_iter().next()
            })
        };
        match real {
            Some(name) => {
                dir.push(&name);
                parts.push(name);
            }
            None => {
                complete = false;
                parts.push(component.to_string());
            }
        }
    }
    (parts.join("/"), complete)
}

/// The on-disk spelling of `content_path` when that FILE exists but only with a different case
/// (`None` when it exists exactly, or in no spelling at all).
pub fn case_variant(addon_root: &Path, content_path: &str) -> Option<String> {
    let (real, complete) = resolve_case(addon_root, content_path);
    (complete && real != content_path).then_some(real)
}

fn map_exists(content_path: &str, addon: &str, didyoumean: Option<String>, detail: &str) -> ToolError {
    let mut error = ToolError::new(
        "MapExists",
        "MapExists",
        format!("{content_path} {detail} in addon {addon:?}; nothing was written"),
    )
    .with_code(-32602)
    .with_data(json!({"path": content_path, "reason": "exists", "addon": addon}))
    .retry_safe(true);
    error = match didyoumean {
        Some(real) => error.did_you_mean([real]),
        None => error.use_instead("open_map", "open the existing map instead", Some(json!({"path": content_path}))),
    };
    error
}

/// What `new_map {clear}` removes from the freshly opened copy of the template.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClearMode {
    None,
    Gameplay,
    All,
}

impl ClearMode {
    fn arg(args: &Value) -> Result<ClearMode, ToolError> {
        match args.get("clear") {
            None | Some(Value::Null) => Ok(ClearMode::None),
            Some(Value::String(s)) => match s.as_str() {
                "none" => Ok(ClearMode::None),
                "gameplay" => Ok(ClearMode::Gameplay),
                "all" => Ok(ClearMode::All),
                other => Err(ToolError::invalid_arguments(format!(
                    "clear must be \"gameplay\", \"all\" or \"none\", got {other:?}"
                ))),
            },
            Some(other) => Err(ToolError::invalid_arguments(format!(
                "clear must be \"gameplay\", \"all\" or \"none\", got {other}"
            ))),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            ClearMode::None => "none",
            ClearMode::Gameplay => "gameplay",
            ClearMode::All => "all",
        }
    }
}

/// Entity classes `clear:"gameplay"` removes: by exact name or prefix. Everything else stays (the
/// environment, the match-flow cameras, and anything this list does not know: a kept node costs
/// the caller one `delete_nodes`, a wrongly removed one is lost work).
const GAMEPLAY_PREFIXES: [&str; 12] = [
    "info_player_", "func_", "trigger_", "prop_", "logic_", "game_", "point_", "weapon_", "item_",
    "hostage", "info_target", "info_hostage",
];

/// The environment, listed for the docs and the tests: these are never removed by `gameplay`
/// (and `point_`/`func_` prefixes above never match them).
pub const ENVIRONMENT_CLASSES: [&str; 12] = [
    "env_sky", "light_environment", "env_cubemap_fog", "env_cubemap", "post_processing_volume",
    "info_map_parameters", "env_soundscape", "env_soundscape_proxy", "env_combined_light_probe_volume",
    "light_probe_volume_from_cubemap", "cs_minimap_boundary", "env_fog_controller",
];

pub fn removed_by_gameplay(class: &str) -> bool {
    GAMEPLAY_PREFIXES.iter().any(|p| class.starts_with(p))
}

/// The nodes `mode` deletes from the parsed template: `(node id, class)` pairs in delete order
/// (meshes count as class `mesh`; the world node is never deleted), and what stays by class.
pub(crate) fn clear_plan(
    doc: &crate::vmap::Document,
    mode: ClearMode,
) -> (Vec<(i64, String)>, std::collections::BTreeMap<String, usize>) {
    use std::collections::BTreeMap;
    let (mut planned, mut kept): (Vec<(i64, String)>, BTreeMap<String, usize>) = Default::default();
    for mesh in doc.meshes() {
        match (mode, mesh.id) {
            (ClearMode::None, _) => {}
            (_, Some(id)) => planned.push((id, "mesh".into())),
            (_, None) => *kept.entry("mesh (no node id)".into()).or_default() += 1,
        }
    }
    for entity in doc.entities() {
        if entity.node_class != "CMapEntity" {
            // the nav data and selection sets of the real template, and the groups and prefabs of
            // an addon-map template: never deleted, so they are reported (worldspawn is the one
            // node "all" keeps by design)
            if entity.node_class != "CMapWorld" {
                *kept.entry(format!("{} ({})", entity.classname, entity.node_class)).or_default() += 1;
            }
            continue;
        }
        let class = if entity.classname.is_empty() { "(none)" } else { entity.classname.as_str() };
        let remove = match mode {
            ClearMode::None => false,
            ClearMode::All => true,
            ClearMode::Gameplay => removed_by_gameplay(class) && !ENVIRONMENT_CLASSES.contains(&class),
        };
        match (remove, entity.node_id) {
            (true, Some(id)) => planned.push((id, class.to_string())),
            _ => *kept.entry(class.to_string()).or_default() += 1,
        }
    }
    (planned, kept)
}

/// Does the active document still look like the one `new_map` just opened? Compares the path
/// (case-insensitive, slash-insensitive) and, when both sides report one, the epoch. `Err` is the
/// reason to skip the clear.
async fn active_is_the_opened(state: &HammerState, ctx: &CallCtx, opened: &Value) -> Result<(), String> {
    let deadline = ctx.budget.deadline_ms().map_err(|e| e.message)?;
    let active = super::live_common::require_document(state, deadline).await.map_err(|e| e.message)?;
    let norm = |v: Option<&Value>| {
        v.and_then(Value::as_str).map(|p| p.replace('\\', "/").to_ascii_lowercase())
    };
    let (want, got) = (norm(opened.get("path")), norm(active.get("path")));
    if want.is_some() && got.is_some() && want != got {
        return Err(format!(
            "the active document is {} (new_map opened {}): its node ids are not the template's",
            got.unwrap_or_default(),
            want.unwrap_or_default()
        ));
    }
    if let (Some(a), Some(b)) = (opened.get("epoch").filter(|e| e.is_number()), active.get("epoch")) {
        if a != b {
            return Err(format!(
                "the active document's epoch is {b} (new_map opened it at {a}): it changed since the open"
            ));
        }
    }
    Ok(())
}

/// Delete the plan's nodes from the open copy through `node.delete` (the op `delete_nodes` sends),
/// in requests of at most 256 ids (one undo group each). Never fails the call: the file exists and
/// is open, so a failure is reported inside `clear`. `removed` counts the chunks that succeeded;
/// `planned` is what was meant to go. The active document is checked against the open first: a
/// tab switch must not delete the template's ids from another map (`skipped`).
async fn apply_clear(
    state: &HammerState,
    ctx: &CallCtx,
    doc: &crate::vmap::Document,
    mode: ClearMode,
    opened: &Value,
) -> Value {
    use std::collections::BTreeMap;
    let (planned, kept) = clear_plan(doc, mode);
    let count = |items: &[(i64, String)]| {
        let mut by: BTreeMap<String, usize> = BTreeMap::new();
        for (_, class) in items {
            *by.entry(class.clone()).or_default() += 1;
        }
        by
    };
    let mut report = json!({
        "mode": mode.name(),
        "planned": {"total": planned.len(), "byClass": count(&planned)},
        "kept": {"total": kept.values().sum::<usize>(), "byClass": kept},
        "undoGroups": 0,
    });
    if planned.is_empty() {
        report["removed"] = json!({"total": 0, "byClass": {}});
        report["deleted"] = json!(0);
        return report;
    }
    if let Err(why) = active_is_the_opened(state, ctx, opened).await {
        report["skipped"] = json!(why);
        report["removed"] = json!({"total": 0, "byClass": {}});
        report["deleted"] = json!(0);
        return report;
    }
    let mut groups = 0u64;
    let mut done: Vec<(i64, String)> = Vec::new();
    for chunk in planned.chunks(super::lifecycle::MAX_NODE_IDS) {
        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        match super::lifecycle::delete_nodes_locked(state, ctx, ids).await {
            Ok(_) => {
                groups += 1;
                done.extend_from_slice(chunk);
            }
            Err(error) => {
                report["error"] = error.to_json();
                report["note"] = json!(format!(
                    "{} of {} nodes were deleted before this failed; list the rest with \
                     find_entities and remove them with delete_nodes (undo_last pops each \
                     deletion group)",
                    done.len(),
                    planned.len()
                ));
                break;
            }
        }
    }
    report["undoGroups"] = json!(groups);
    report["deleted"] = json!(done.len());
    report["removed"] = json!({"total": done.len(), "byClass": count(&done)});
    if report.get("error").is_none() {
        report["note"] = json!(if groups > 1 {
            "removed in several requests, one undo group each: undo_last once per group to restore"
        } else {
            "removed in one undo group: undo_last restores it"
        });
    }
    report
}

pub async fn new_map(state: HammerState, ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    // one document-creating call at a time, and the open below happens under the same gate
    let _gate = state.write_gate().await;
    let content_path = super::lifecycle::map_content_path(&args)?;
    let open = bool_arg(&args, "open", true)?;
    let overwrite = bool_arg(&args, "overwrite", false)?;
    let clear = ClearMode::arg(&args)?;
    if clear != ClearMode::None && !open {
        return Err(ToolError::invalid_arguments(
            "clear deletes the template's nodes in the open document: it needs open (not open:false)",
        ));
    }
    let template_arg = match args.get("template") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(t.trim().to_string()),
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!("template: a string, got {other}")))
        }
    };

    let content = state.content();
    let addon = content.addon.clone();
    let addon_root = content.addon_root();
    if addon.eq_ignore_ascii_case(TEMPLATE_ADDON) {
        return Err(ToolError::new(
            "InvalidArguments",
            "InvalidArguments",
            format!(
                "the selected addon is Valve's {TEMPLATE_ADDON}, the pristine copy every new workshop \
                 addon starts from: new_map never writes into it. set_addon to your own addon"
            ),
        )
        .with_code(-32602)
        .with_data(json!({"addon": addon}))
        .retry_safe(true));
    }
    if !addon_root.is_dir() {
        let available = super::addon::available_addons(&state);
        return Err(ToolError::new(
            "AddonNotFound",
            "AddonNotFound",
            format!(
                "no addon {addon:?} under content/csgo_addons (available: {}); new_map writes \
                 into an existing addon (create it with the workshop tools, or set_addon to one \
                 that exists)",
                if available.is_empty() { "none".to_string() } else { available.join(", ") }
            ),
        )
        .with_code(-32602)
        .with_data(json!({"addon": addon, "available": available}))
        .retry_safe(true));
    }
    let maps_root = addon_root.join("maps");
    let target = addon_root.join(&content_path);
    let target_parent = target.parent().map(Path::to_path_buf).unwrap_or_else(|| maps_root.clone());
    inside_maps(&maps_root, &target_parent)?;

    // the template: resolved and parsed BEFORE anything is written
    let (template_name, template_host, template_content) = match template_arg.as_deref() {
        None | Some("") | Some("addon_template") | Some("default") => {
            let Some(host) = default_template(&state) else {
                return Err(ToolError::new(
                    "AssetNotFound",
                    "AssetNotFound",
                    format!(
                        "Valve's template map is not under {} (content/csgo_addons/{TEMPLATE_ADDON}/maps/): \
                         is the game install mounted? Pass template:\"maps/<existing>.vmap\" to copy \
                         a map of the active addon instead",
                        state.app_root().display()
                    ),
                )
                .with_code(-32014)
                .with_data(json!({"reason": "missing_template", "template": "addon_template"}))
                .retry_safe(true));
            };
            (TEMPLATE_ADDON.to_string(), host, None)
        }
        Some("empty") => {
            return Err(ToolError::invalid_arguments(
                "template: \"empty\" is not offered (an empty world is not proven loadable by \
                 Hammer); omit template for Valve's addon_template map, or name an existing \
                 maps/<x>.vmap of the active addon",
            ))
        }
        Some(other) => {
            let path = super::lifecycle::map_content_path(&json!({"path": other})).map_err(|e| {
                ToolError::invalid_arguments(format!(
                    "template: {} (omit it for addon_template, or pass an existing maps/<x>.vmap of \
                     the active addon)",
                    e.message.trim_start_matches("path: ")
                ))
            })?;
            let host = addon_root.join(&path);
            if !host.is_file() {
                let (names, _) = addon_maps(&state, 200);
                let near = nearest_names(&path, names.iter().map(String::as_str), 5);
                return Err(ToolError::new(
                    "AssetNotFound",
                    "AssetNotFound",
                    format!(
                        "template {path} is not a file under {} (addon {addon:?}); nothing was \
                         written. Omit template for Valve's addon_template map",
                        addon_root.display()
                    ),
                )
                .with_code(-32014)
                .did_you_mean(near)
                .with_data(json!({"reason": "missing_template", "template": path,
                                  "addonMaps": names.iter().take(STATUS_MAP_CAP).collect::<Vec<_>>()}))
                .retry_safe(true));
            }
            (path.clone(), host, Some(path))
        }
    };
    let same_file = template_host == target
        || (target.exists()
            && std::fs::canonicalize(&template_host).ok() == std::fs::canonicalize(&target).ok());
    if same_file {
        return Err(ToolError::invalid_arguments(
            "template and path are the same file: nothing to copy",
        ));
    }
    let parsed = crate::vmap::load(&template_host).map_err(|error| {
        ToolError::new(
            "InvalidMapFile",
            "InvalidMapFile",
            format!(
                "the template {} is not a readable binary-v9 .vmap ({error}); nothing was written",
                template_host.display()
            ),
        )
        .with_data(json!({"template": template_name, "reason": "InvalidMapFile"}))
        .retry_safe(true)
    })?;
    let bytes = std::fs::read(&template_host).map_err(|error| {
        ToolError::new(
            "AssetNotFound",
            "AssetNotFound",
            format!("cannot read the template {}: {error}", template_host.display()),
        )
        .with_code(-32014)
        .retry_safe(true)
    })?;
    let contents = contents_of(&parsed, bytes.len());

    // the target. Case first: Hammer (Wine) treats maps/Foo.vmap and maps/foo.vmap as one file, so a
    // path that differs from an existing file or directory only in case would make a duplicate
    // that Hammer resolves either way; refused whatever `overwrite` says
    let (real_spelling, _) = resolve_case(&addon_root, &content_path);
    if real_spelling != content_path {
        return Err(map_exists(
            &content_path,
            &addon,
            Some(real_spelling),
            "differs only in case from an existing file or directory (this host is case-sensitive, \
             Hammer is not; use the real spelling in didYouMean)",
        ));
    }
    if std::fs::symlink_metadata(&target).is_ok() && !overwrite {
        return Err(map_exists(
            &content_path,
            &addon,
            None,
            "already exists (pass overwrite:true to replace it, the old file is kept as a .bak copy; \
             or open the existing map with open_map)",
        ));
    }
    if target.is_dir() {
        return Err(ToolError::invalid_arguments(format!("{content_path} is a directory")));
    }
    // overwrite of the map Hammer has open: the editor would keep the OLD document and `save_map`
    // would write it over the fresh copy
    if overwrite && std::fs::symlink_metadata(&target).is_ok() {
        let active = match ctx.budget.deadline_ms() {
            Ok(deadline) => crate::tools::live_common::read_active_document(&state, Some(deadline))
                .await
                .ok()
                .flatten(),
            Err(_) => None,
        };
        if active.as_ref().is_some_and(|doc| super::lifecycle::document_is(doc, &content_path)) {
            return Err(ToolError::new(
                "MapOpenInHammer",
                "MapOpenInHammer",
                format!(
                    "{content_path} is the document open in Hammer: replacing the file under it would \
                     leave the editor on the old contents, and save_map would write them back over \
                     the copy. Nothing was written. Pick another path, or have the map closed first \
                     (no tool closes a tab)"
                ),
            )
            .with_code(-32602)
            .with_data(json!({"path": content_path, "reason": "active_document"}))
            .retry_safe(true));
        }
    }
    if open {
        // a first-map open_asset still in flight: open_map would refuse it AFTER the file was written
        if let Some(error) = super::lifecycle::open_pending_refusal(&state) {
            return Err(error);
        }
    }
    // open:true goes through map.open / open_asset, which resolve under the ENGINE's addon: a
    // mismatch is refused now, before a file is written that could not be opened
    if open {
        let (engine_addon, engine_source) = super::lifecycle::engine_addon(&state).await;
        if !addon.eq_ignore_ascii_case(&engine_addon) {
            return Err(super::lifecycle::wrong_engine_addon(
                &content_path,
                &addon,
                &engine_addon,
                engine_source,
            )
            .with_data(json!({"hint": "pass open:false to only create the file in the selected addon"})));
        }
    }

    if let Err(error) = std::fs::create_dir_all(&target_parent) {
        return Err(ToolError::internal(format!(
            "cannot create {}: {error}",
            target_parent.display()
        )));
    }
    // a symlink made between the check and the mkdir would be caught here
    inside_maps(&maps_root, &target_parent)?;
    let written = if overwrite {
        crate::vmdl::fsutil::write_atomic_bytes_with_backup(&target, &bytes)
    } else {
        // never replaces: a file made since the check (Hammer's own Save As takes no gate of ours) wins
        crate::vmdl::fsutil::write_atomic_bytes_new(&target, &bytes).map(|()| None)
    };
    let backup = written.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return map_exists(&content_path, &addon, None, "was created while new_map was working");
        }
        ToolError::internal(format!("writing {} failed: {error}", target.display()))
            .with_data(json!({"path": content_path, "created": false}))
    })?;

    let mut created = Map::new();
    created.insert("created".into(), json!(true));
    created.insert("path".into(), json!(content_path));
    created.insert(
        "template".into(),
        json!({
            "name": template_name,
            "source": template_host.display().to_string(),
            "kind": if template_content.is_some() { "addon map" } else { "valve template" },
        }),
    );
    created.insert("contents".into(), contents);
    if template_content.is_none() {
        // Valve's template: bring along what it references from the template addon
        created.insert(
            "templateAssets".into(),
            super::new_map_assets::bring_template_assets(&state.app_root(), &addon, &bytes),
        );
    }
    created.insert("addon".into(), json!(addon));
    created.insert("hostPath".into(), json!(target.display().to_string()));
    if let Some(backup) = &backup {
        created.insert("replaced".into(), json!({"backup": backup.display().to_string()}));
    }

    if !open {
        let mut out = created;
        out.insert("opened".into(), json!(false));
        return Ok(ToolOutput::from(Value::Object(out)).with_next_args(
            "open_map",
            "open the new map in Hammer",
            json!({"path": content_path}),
        ));
    }

    let opened = super::lifecycle::open_map_locked(&state, &ctx, &json!({"path": content_path})).await;
    match opened {
        Ok(mut output) => {
            if let Some(map) = output.json.as_object_mut() {
                for (key, value) in created {
                    // `path` stays open_map's (the document's own path); ours is `contentPath`
                    if key != "path" {
                        map.insert(key, value);
                    }
                }
            }
            let existing = output.json.get("existing").and_then(Value::as_bool) == Some(true);
            if clear != ClearMode::None {
                let report = if existing {
                    json!({"mode": clear.name(), "skipped": "Hammer already had this file open and kept its old document, so its nodes are not the template's"})
                } else {
                    apply_clear(&state, &ctx, &parsed, clear, &output.json).await
                };
                if let Some(map) = output.json.as_object_mut() {
                    map.insert("clear".into(), report);
                }
            }
            if existing {
                // Hammer already had this file open: it kept its old document
                let active = output.json.get("active").and_then(Value::as_bool) == Some(true);
                if let Some(map) = output.json.as_object_mut() {
                    map.insert("editorStale".into(), json!(true));
                    map.insert(
                        "createdNote".into(),
                        json!("the file was replaced on disk, but Hammer still holds the OLD document \
                               (it was already open): save_map would write the old contents back"),
                    );
                }
                output.next = Some(Vec::new());
                if active {
                    output = output.with_next_args(
                        "reload_map",
                        "re-read the replaced file into the editor (force:true drops the old document)",
                        json!({"force": true}),
                    );
                }
            } else if output.next.is_none() {
                output = output
                    .with_next(
                        "apply_layout",
                        "build geometry and entities in the new map (create_block / create_entities for single items)",
                    )
                    .with_next("save_map", "persist the live edits to the .vmap (nothing is auto-saved)");
            }
            Ok(output)
        }
        Err(mut error) => {
            // the file exists now whatever happened to the open
            error.data.insert("created".into(), json!(true));
            error.data.insert("createdPath".into(), json!(content_path));
            error.data.insert(
                "createdNote".into(),
                json!("the map file was written before the open failed; it is still there"),
            );
            if !error.outcome_unknown
                && !error.data.contains_key("useInstead")
                && !error.data.contains_key("retryWith")
            {
                error = error.use_instead(
                    "open_map",
                    "the file was created: retry the open",
                    Some(json!({"path": content_path})),
                );
            }
            Err(error)
        }
    }
}
