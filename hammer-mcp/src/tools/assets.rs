//! `find_assets` (streamline P5): one finder over the addon and the game paks for the kinds a
//! map author picks by name: materials, models, particles, sounds (`.vsnd`) and textures.
//! `list_models` stays the model-only lister; models here come from the same catalog
//! (`model_catalog.rs`), the other kinds from per-kind indexes built once from the two game paks
//! (cached by pak mtime+len like the VPK cache) plus a fresh bounded walk of the addon's
//! compiled and source trees on every call (the addon changes during a session).
//!
//! A hit's `path` is the form the other tools take (lowercase, `/`, the source extension:
//! `materials/dev/reflectivity_30.vmat`). `source` is the highest-precedence place holding it:
//! `addon` (compiled in the addon), `content` (addon source only, not compiled), `csgo`, `core`.
//! A material hit on the returned page also says whether it renders, through the one resolver
//! `report_ready` uses ([`super::asset_resolve::Sources::check_textures`]): `ok` (compiled and
//! every `.vtex` it names resolves), `missing_textures` (+ which: those faces render magenta) or
//! `unknown` (+ why: not compiled, unreadable, or a game pak could not be read). `ok` is only
//! ever said after the texture list was read and resolved.
use super::asset_resolve::{Paths, Sources};
use super::model_catalog::{catalog_for, walk_files_bounded, ModelSource};
use crate::state::HammerState;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

const LIMIT_DEFAULT: usize = 25;
const LIMIT_MAX: usize = 200;
const KNOWN_ARGS: &[&str] = &["kind", "query", "prefix", "source", "limit", "cursor"];
/// How many missing textures one hit lists.
const MISSING_LIST_MAX: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Material,
    Model,
    Particle,
    Sound,
    Texture,
}

impl Kind {
    fn parse(text: &str) -> Option<Kind> {
        Some(match text {
            "material" => Kind::Material,
            "model" => Kind::Model,
            "particle" => Kind::Particle,
            "sound" => Kind::Sound,
            "texture" => Kind::Texture,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Material => "material",
            Kind::Model => "model",
            Kind::Particle => "particle",
            Kind::Sound => "sound",
            Kind::Texture => "texture",
        }
    }

    /// The directory the kind lives under (`""` = anywhere: textures sit next to materials and
    /// elsewhere).
    fn dir(self) -> &'static str {
        match self {
            Kind::Material => "materials",
            Kind::Model => "models",
            Kind::Particle => "particles",
            Kind::Sound => "sounds",
            Kind::Texture => "",
        }
    }

    /// The source extension (the compiled one is this plus `_c`).
    fn ext(self) -> &'static str {
        match self {
            Kind::Material => "vmat",
            Kind::Model => "vmdl",
            Kind::Particle => "vpcf",
            Kind::Sound => "vsnd",
            Kind::Texture => "vtex",
        }
    }
}

/// Source labels in precedence order (index = rank).
const SOURCES: [&str; 4] = ["addon", "content", "csgo", "core"];

// -------------------------------------------------------------------------------------------
// the game pak index (built once per pak mtime)
// -------------------------------------------------------------------------------------------

/// Canonical source-spelled paths of one kind in the game paks: `(path, pak rank)`, sorted.
type KindIndex = Vec<(String, u8)>;

struct GameIndex {
    key: (PathBuf, Option<SystemTime>, u64, Option<SystemTime>, u64),
    kinds: HashMap<Kind, KindIndex>,
}

fn game_index(paths: &Paths) -> Result<Arc<GameIndex>, String> {
    static CACHE: OnceLock<Mutex<Option<Arc<GameIndex>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let csgo_path = paths.app_root.join("game/csgo/pak01_dir.vpk");
    let core_path = paths.app_root.join("game/core/pak01_dir.vpk");
    let stat = |p: &std::path::Path| std::fs::metadata(p).map(|m| (m.modified().ok(), m.len()));
    let (cm, cl) = stat(&csgo_path).map_err(|e| format!("{}: {e}", csgo_path.display()))?;
    let (om, ol) = stat(&core_path).map_err(|e| format!("{}: {e}", core_path.display()))?;
    let key = (paths.app_root.clone(), cm, cl, om, ol);
    if let Some(index) = cache.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        if index.key == key {
            return Ok(Arc::clone(index));
        }
    }
    let csgo = super::asset_resolve::open_vpk(&csgo_path)?;
    let core = super::asset_resolve::open_vpk(&core_path)?;
    let mut by_kind: HashMap<Kind, BTreeMap<String, u8>> = HashMap::new();
    for (rank, pak) in [(2u8, &csgo), (3u8, &core)] {
        for path in pak.paths() {
            let Some(source) = path.strip_suffix("_c") else { continue };
            let Some((_, ext)) = source.rsplit_once('.') else { continue };
            let kind = match ext {
                "vmat" if path.starts_with("materials/") => Kind::Material,
                "vpcf" if path.starts_with("particles/") => Kind::Particle,
                "vsnd" if path.starts_with("sounds/") => Kind::Sound,
                "vtex" => Kind::Texture,
                _ => continue,
            };
            // csgo is visited first: the first pak to hold a path keeps it
            by_kind.entry(kind).or_default().entry(source.to_string()).or_insert(rank);
        }
    }
    let kinds = by_kind
        .into_iter()
        .map(|(kind, map)| (kind, map.into_iter().collect::<KindIndex>()))
        .collect();
    let index = Arc::new(GameIndex { key, kinds });
    *cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&index));
    Ok(index)
}

// -------------------------------------------------------------------------------------------
// matching
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFilter {
    All,
    Addon,
    Game,
}

struct Filter {
    query: Option<String>,
    prefix: Option<String>,
    source: SourceFilter,
    cursor: Option<String>,
}

impl Filter {
    fn keeps(&self, path: &str) -> bool {
        self.prefix.as_ref().is_none_or(|p| path.starts_with(p.as_str()))
            && self.query.as_ref().is_none_or(|q| path.contains(q.as_str()))
    }
}

#[derive(Debug, Clone)]
struct Found {
    rank: u8,
    compiled: bool,
}

fn add(found: &mut BTreeMap<String, Found>, path: String, rank: u8, compiled: bool) {
    let slot = found.entry(path).or_insert(Found { rank, compiled });
    slot.rank = slot.rank.min(rank);
    slot.compiled |= compiled;
}

/// Every match of `kind`, by canonical path. Blocking (pak iteration, addon walks).
fn collect(paths: &Paths, kind: Kind, filter: &Filter) -> Result<(BTreeMap<String, Found>, Vec<String>), String> {
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();
    if filter.source != SourceFilter::Game {
        let ext = kind.ext();
        let walks = [
            (paths.game.clone(), format!(".{ext}_c"), 0u8, true),
            (paths.content.clone(), format!(".{ext}"), 1u8, false),
        ];
        for (root, suffix, rank, compiled) in walks {
            let dir = if kind.dir().is_empty() { root.clone() } else { root.join(kind.dir()) };
            if !dir.is_dir() {
                continue;
            }
            let lead = if kind.dir().is_empty() { String::new() } else { format!("{}/", kind.dir()) };
            for relative in walk_files_bounded(&dir, &[suffix.as_str()], kind.name(), &mut notes) {
                let mut path = format!("{lead}{relative}").to_ascii_lowercase();
                if compiled {
                    path.truncate(path.len() - 2);
                }
                if filter.keeps(&path) {
                    add(&mut found, path, rank, compiled);
                }
            }
        }
    }
    if filter.source != SourceFilter::Addon {
        let index = game_index(paths)?;
        if let Some(entries) = index.kinds.get(&kind) {
            for (path, rank) in entries {
                if filter.keeps(path) {
                    add(&mut found, path.clone(), *rank, true);
                }
            }
        }
    }
    Ok((found, notes))
}

// -------------------------------------------------------------------------------------------
// the tool
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "find_assets",
            "Find assets in the addon and the game paks by kind: material, model, particle, sound (.vsnd), texture. Hits: `path` (use verbatim), `source` (addon|content=uncompiled source|csgo|core), `compiled`; materials add `renders`: ok | missing_textures (+missingTextures, renders magenta) | unknown (+reason). Sorted by path; limit default 25; pass `nextCursor` as `cursor` for more; `total` counts every match. Prefer renders:ok. Models: get_model_info next.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["material", "model", "particle", "sound", "texture"]},
                "query": {"type": "string", "description": "case-insensitive substring of the path"},
                "prefix": {"type": "string", "description": "path prefix, e.g. 'materials/dev/'"},
                "source": {"type": "string", "enum": ["all", "addon", "game"]},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIMIT_MAX},
                "cursor": {"type": "string", "description": "the previous reply's nextCursor"}
            },
            "required": ["kind"],
            "additionalProperties": false
        })),
        find_assets,
    );
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!("{name} must be a string, got {other}"))),
    }
}

fn norm_path(text: &str) -> String {
    text.trim().replace('\\', "/").trim_start_matches('/').to_ascii_lowercase()
}

/// How one material page entry renders. `scope` is the caller's `source` filter: the wording of
/// "not compiled" must not claim a search the filter skipped.
fn renders(sources: &Sources, key: &str, compiled: bool, scope: SourceFilter, seen: &mut HashMap<String, bool>) -> Value {
    if !compiled {
        let reason = match scope {
            SourceFilter::Addon => "not compiled in the addon (source only): compile_asset it first",
            _ => "no compiled .vmat_c anywhere (source only): compile_asset it first",
        };
        return json!({"renders": "unknown", "reason": reason});
    }
    if !sources.pak_errors.is_empty() {
        return json!({"renders": "unknown", "reason": format!("a game pak could not be read to resolve textures: {}", sources.pak_errors.join("; "))});
    }
    match sources.check_textures(key, seen) {
        Err(why) => json!({"renders": "unknown", "reason": why}),
        Ok((_, missing)) if missing.is_empty() => json!({"renders": "ok"}),
        Ok((_, missing)) => json!({
            "renders": "missing_textures",
            "missingTextures": missing.iter().take(MISSING_LIST_MAX).collect::<Vec<_>>(),
        }),
    }
}

pub async fn find_assets(state: HammerState, _ctx: CallCtx, args: Value) -> Result<ToolOutput, ToolError> {
    let Value::Object(map) = &args else {
        return Err(ToolError::invalid_arguments(format!("arguments must be an object, got {args}")));
    };
    for key in map.keys() {
        if !KNOWN_ARGS.contains(&key.as_str()) {
            return Err(ToolError::invalid_arguments(format!(
                "unknown argument {key:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    let kind = match string_arg(&args, "kind")? {
        Some(text) => Kind::parse(text).ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "kind must be material, model, particle, sound or texture, got {text:?}"
            ))
        })?,
        None => return Err(ToolError::invalid_arguments("kind is required (material, model, particle, sound, texture)")),
    };
    let query = match string_arg(&args, "query")? {
        Some("") => return Err(ToolError::invalid_arguments("query is empty (omit it or give a substring)")),
        other => other.map(norm_path),
    };
    let prefix = match string_arg(&args, "prefix")? {
        Some("") => return Err(ToolError::invalid_arguments("prefix is empty (omit it)")),
        other => other.map(norm_path),
    };
    let source = match string_arg(&args, "source")? {
        None | Some("all") => SourceFilter::All,
        Some("addon") => SourceFilter::Addon,
        Some("game") => SourceFilter::Game,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "source must be \"all\", \"addon\" or \"game\", got {other:?}"
            )))
        }
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => LIMIT_DEFAULT,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(l) if (1..=LIMIT_MAX as u64).contains(&l) => l as usize,
            _ => return Err(ToolError::invalid_arguments(format!("limit must be an integer in 1..={LIMIT_MAX}, got {n}"))),
        },
        Some(other) => return Err(ToolError::invalid_arguments(format!("limit must be an integer in 1..={LIMIT_MAX}, got {other}"))),
    };
    let filter = Filter {
        query,
        prefix,
        source,
        cursor: string_arg(&args, "cursor")?.map(norm_path),
    };

    let paths = Paths::of(&state);
    // models: the catalog (its addon part rescanned for this call), the rest: the indexes
    let (found, mut notes) = if kind == Kind::Model {
        // the cached catalog (pak part, rebuilt only when the csgo pak's mtime moves) plus a
        // fresh walk of the addon's model dirs only
        let catalog = catalog_for(&state).await?;
        let filter_ref = Filter { query: filter.query.clone(), prefix: filter.prefix.clone(), source, cursor: None };
        tokio::task::spawn_blocking(move || {
            let (addon, notes) = catalog.addon_scan();
            let mut found: BTreeMap<String, Found> = BTreeMap::new();
            if filter_ref.source != SourceFilter::Addon {
                for entry in catalog.entries().filter(|e| filter_ref.keeps(&e.path)) {
                    let paks = entry.sources.iter().filter_map(|s| match s {
                        ModelSource::Vpk { pak } => Some(if *pak == "csgo" { 2u8 } else { 3 }),
                        _ => None,
                    });
                    if let Some(rank) = paks.min() {
                        add(&mut found, entry.path.clone(), rank, true);
                    }
                }
            }
            if filter_ref.source != SourceFilter::Game {
                for (path, sources) in addon.iter().filter(|(p, _)| filter_ref.keeps(p)) {
                    for source in sources {
                        match source {
                            ModelSource::AddonGame { .. } => add(&mut found, path.clone(), 0, true),
                            ModelSource::AddonContent { .. } => add(&mut found, path.clone(), 1, false),
                            ModelSource::Vpk { .. } => {}
                        }
                    }
                }
            }
            (found, notes)
        })
        .await
        .map_err(|e| ToolError::internal(format!("find_assets: the model scan task failed: {e}")))?
    } else {
        let walk_paths = paths.clone();
        let walk_filter = Filter { query: filter.query.clone(), prefix: filter.prefix.clone(), source, cursor: None };
        tokio::task::spawn_blocking(move || collect(&walk_paths, kind, &walk_filter))
            .await
            .map_err(|e| ToolError::internal(format!("find_assets: the index task failed: {e}")))?
            .map_err(|e| {
                ToolError::new("VpkMissing", "VpkMissing", format!("the game pak index cannot be built: {e}"))
                    .retry_safe(true)
            })?
    };

    let total = found.len();
    let mut page: Vec<(String, Found)> = found
        .into_iter()
        .filter(|(path, _)| filter.cursor.as_ref().is_none_or(|c| path > c))
        .take(limit + 1)
        .collect();
    let more = page.len() > limit;
    page.truncate(limit);
    let next_cursor = more.then(|| page.last().map(|(p, _)| p.clone())).flatten();

    // per-hit material checks: only the returned page, off the async thread
    let page_json: Vec<Value> = if kind == Kind::Material {
        let paths = paths.clone();
        tokio::task::spawn_blocking(move || {
            let mut sources = Sources::new(&paths, None);
            if source == SourceFilter::Game {
                // a hit labelled with a pak must be checked against that pak, not an addon override
                sources.dirs.remove(0);
            }
            let mut seen = HashMap::new();
            page.into_iter()
                .map(|(path, f)| {
                    let mut hit = json!({"path": path, "source": SOURCES[f.rank as usize], "compiled": f.compiled});
                    if let (Some(h), Some(r)) = (hit.as_object_mut(), renders(&sources, &path, f.compiled, source, &mut seen).as_object()) {
                        h.extend(r.clone());
                    }
                    hit
                })
                .collect()
        })
        .await
        .map_err(|e| ToolError::internal(format!("find_assets: the material check task failed: {e}")))?
    } else {
        page.into_iter()
            .map(|(path, f)| json!({"path": path, "source": SOURCES[f.rank as usize], "compiled": f.compiled}))
            .collect()
    };

    let kind_name = args["kind"].as_str().unwrap_or_default();
    let mut out = json!({
        "kind": kind_name,
        "total": total,
        "returned": page_json.len(),
        "hits": page_json,
        "addon": state.addon(),
    });
    if let Some(cursor) = &next_cursor {
        out["nextCursor"] = json!(cursor);
    }
    if total == 0 {
        notes.push("no asset of this kind matches (query is a substring of the path; try a shorter one or drop prefix/source)".into());
    }
    if !notes.is_empty() {
        out["note"] = json!(notes.join("; "));
    }
    let hits = out["hits"].as_array().cloned().unwrap_or_default();
    let mut output: ToolOutput = out.into();
    match kind {
        Kind::Model => {
            let path_of = |h: &Value| h["path"].as_str().map(str::to_string);
            if let Some(path) = hits.iter().find(|h| h["compiled"] == true).and_then(path_of) {
                output = output.with_next_args(
                    "get_model_info",
                    "bounds, facing and attachments of a compiled hit before placing it",
                    json!({"model": path}),
                );
            } else if let Some(path) = hits.first().and_then(path_of) {
                output = output.with_next_args(
                    "model_compile",
                    "no hit on this page is compiled: compile the first one (a prop needs a .vmdl_c)",
                    json!({"path": path}),
                );
            }
        }
        Kind::Material => {
            // set_mesh_material needs a nodeID this tool does not have: the material goes in the
            // why, no args are offered
            if let Some(ok) = hits.iter().find(|h| h["renders"] == "ok").and_then(|h| h["path"].as_str()) {
                output = output.with_next(
                    "set_mesh_material",
                    &format!("paint a mesh (nodeID, material) with {ok}, which renders"),
                );
            }
        }
        _ => {}
    }
    Ok(output)
}
