//! The model catalog (WP-V): every model a prop may name (canonical `"models/**.vmdl"`), from
//! the game VPKs + the current addon, and the `list_models` read-only tool over it. The catalog
//! is built once, cached on [`HammerState::model_catalog`] inside `tokio::task::spawn_blocking`
//! (the VPK parses and the addon walks are blocking), refreshed when the csgo `pak01_dir.vpk`
//! mtime moves and rescanned (addon dirs only, VPK trees reused) when a lookup misses — the
//! addon compiles into it during a session.
//!
//! Sources and precedence (same canonical path through every route; an entry's `sources` vec is
//! sorted into this order, so an addon copy of a game model outranks the game VPK — Hammer
//! renders what the addon recompiled):
//! * `game/csgo_addons/<addon>/models/**/*.vmdl_c` — compiled addon models;
//! * `content/csgo_addons/<addon>/models/**/*.vmdl` — sources (may be uncompiled);
//! * the game VPKs: `game/csgo/pak01_dir.vpk`, then `game/core/pak01_dir.vpk` (vmdl_c entries),
//!   through `crate::vpk`.
//!
//! The canonical model name is the `model` keyvalue spelling: lowercase, `/` separators,
//! `.vmdl` even when the file is `.vmdl_c`, always under `models/`. [`normalize_model_path`]
//! accepts the compiled spelling, backslashes and case and rejects absolute paths, `..`,
//! non-`models/` paths and non-model extensions. Missing or invalid input is an error naming
//! the file — neither an empty success nor a guess (the absent addon directories, a walk that
//! hit a bound and a missing game VPK are on the record: notes and errors, never silence).
use crate::state::HammerState;
use crate::vpk::{VpkDir, VpkError};
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// The catalog's cached value sits on [`HammerState::model_catalog`] (see its doc comment).
const PLAN: &str = "target/WP.md (model catalog)";
/// The addon walk's directory bound: deeper directories are not entered, and the scan notes it.
const WALK_MAX_DEPTH: usize = 16;
/// The addon walk's file bound: past it the scan stops and notes (never an unbounded walk).
const WALK_MAX_FILES: usize = 200_000;
/// `list_models`' default and maximum `limit`.
const LIMIT_DEFAULT: u64 = 100;
const LIMIT_MAX: u64 = 1000;

// -------------------------------------------------------------------------------------------
// the catalog's data model
// -------------------------------------------------------------------------------------------

/// Where one model's files live; the vec is in precedence order and the wire labels are
/// [`ModelSource::label`] (`addon-game`, `addon-content`, `vpk:csgo`, `vpk:core`). The `file`
/// fields keep the on-disk spelling as the walk saw it (the canonical name is lowercased, the
/// file name keeps its case — reads open the walked path, never one rebuilt from the
/// lowercased canonical).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// `<app>/game/csgo_addons/<addon>/models/**` — the addon's compiled model file (`file` is
    /// relative to the addon `models` root).
    AddonGame { file: PathBuf },
    /// `<app>/content/csgo_addons/<addon>/models/**` — the addon's `.vmdl` source (never read
    /// for compiled bytes).
    AddonContent { file: PathBuf },
    /// The game's directory VPK (`pak` is `"csgo"` or `"core"`).
    Vpk { pak: &'static str },
}

/// Precedence rank of a source (lower wins): the addon's compiled file, then its content
/// source, then the game VPKs (csgo before core).
fn rank(source: &ModelSource) -> u8 {
    match source {
        ModelSource::AddonGame { .. } => 0,
        ModelSource::AddonContent { .. } => 1,
        ModelSource::Vpk { pak } if *pak == "csgo" => 2,
        ModelSource::Vpk { .. } => 3,
    }
}

impl ModelSource {
    /// The wire label a `list_models` entry's `sources` array carries.
    pub fn label(&self) -> &'static str {
        match self {
            ModelSource::AddonGame { .. } => "addon-game",
            ModelSource::AddonContent { .. } => "addon-content",
            ModelSource::Vpk { pak } => match *pak {
                "csgo" => "vpk:csgo",
                _ => "vpk:core",
            },
        }
    }
}

/// One canonical model path and everything that carries it (`sources` in precedence order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// Canonical spelling: lowercase, `/` separated, `.vmdl` (the `model` keyvalue form).
    pub path: String,
    pub sources: Vec<ModelSource>,
    /// Whether a compiled `.vmdl_c` route exists (an addon game file or a game VPK entry).
    pub compiled: bool,
}

/// A failed catalog build: a missing game VPK names the file (a half catalog is never built).
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("the game's directory VPK is missing: {path}")]
    MissingVpk { path: String },
    #[error("the game's VPK could not be read: {0}")]
    Vpk(#[from] VpkError),
}

/// The built catalog: sorted canonical entries, the two parsed VPK trees (so a rescan or a read
/// needs no re-parse) and the scan's notes (a missing addon directory, a walk that hit a bound).
#[derive(Debug)]
pub struct ModelCatalog {
    entries: BTreeMap<String, ModelEntry>,
    app_root: PathBuf,
    addon: String,
    csgo: Arc<VpkDir>,
    core: Arc<VpkDir>,
    csgo_mtime: Option<SystemTime>,
    notes: Vec<String>,
}

impl ModelCatalog {
    /// The addon this catalog was built for.
    pub fn addon(&self) -> &str {
        &self.addon
    }

    /// Build the catalog for the app tree (`<project>/workshop/run/app`) and the addon. Both
    /// game VPKs are required ([`CatalogError::MissingVpk`] names the missing file), the addon
    /// directories may legitimately not exist (that is a note on the result, never silence).
    pub fn build(app_root: &Path, addon: &str) -> Result<ModelCatalog, CatalogError> {
        let csgo_path = app_root.join("game/csgo/pak01_dir.vpk");
        if !csgo_path.is_file() {
            return Err(CatalogError::MissingVpk {
                path: csgo_path.display().to_string(),
            });
        }
        let core_path = app_root.join("game/core/pak01_dir.vpk");
        if !core_path.is_file() {
            return Err(CatalogError::MissingVpk {
                path: core_path.display().to_string(),
            });
        }
        let csgo = Arc::new(VpkDir::open(&csgo_path)?);
        let core = Arc::new(VpkDir::open(&core_path)?);
        let csgo_mtime = file_mtime(&csgo_path);
        Ok(Self::assemble(app_root, addon, csgo, core, csgo_mtime))
    }

    /// The catalog from already-parsed parts, then the addon walks on top. Shared by
    /// [`ModelCatalog::build`] and [`ModelCatalog::rescan_addon`].
    fn assemble(
        app_root: &Path,
        addon: &str,
        csgo: Arc<VpkDir>,
        core: Arc<VpkDir>,
        csgo_mtime: Option<SystemTime>,
    ) -> ModelCatalog {
        let mut catalog = ModelCatalog {
            entries: BTreeMap::new(),
            app_root: app_root.to_path_buf(),
            addon: addon.to_string(),
            csgo,
            core,
            csgo_mtime,
            notes: Vec::new(),
        };
        // The game VPKs (they cannot produce notes), then the addon walks (they note); the
        // parts are pushed in whatever order, the sort below puts every entry's `sources` into
        // the precedence order (`rank`).
        let paks = [
            ("csgo", Arc::clone(&catalog.csgo)),
            ("core", Arc::clone(&catalog.core)),
        ];
        for (pak, vpk) in &paks {
            for path in vpk.paths() {
                if (path.starts_with("models/") || path.starts_with("agents/models/")) && path.ends_with(".vmdl_c") {
                    let canonical = path[..path.len() - 2].to_string();
                    catalog
                        .sources_of(&canonical)
                        .push(ModelSource::Vpk { pak });
                }
            }
        }
        let (addon, notes) = catalog.addon_scan();
        catalog.notes.extend(notes);
        for (canonical, sources) in addon {
            catalog.sources_of(&canonical).extend(sources);
        }
        for entry in catalog.entries.values_mut() {
            // Precedence order: the addon's recompiled file outranks the game VPK copy (Hammer
            // renders the addon's — `read_compiled` walks this order too). Stable sort: the
            // VPK paks keep their assembly order csgo-then-core.
            entry.sources.sort_by_key(rank);
            entry.compiled = entry.sources.iter().any(|source| {
                matches!(
                    source,
                    ModelSource::AddonGame { .. } | ModelSource::Vpk { .. }
                )
            });
        }
        catalog
    }

    /// A fresh walk of the addon's compiled and source model dirs only (the VPK trees are not
    /// touched): canonical path -> its addon sources, plus the walk's notes. `find_assets` merges
    /// this with the cached catalog's pak entries so a call never rebuilds the whole catalog.
    pub(crate) fn addon_scan(&self) -> (BTreeMap<String, Vec<ModelSource>>, Vec<String>) {
        let mut found: BTreeMap<String, Vec<ModelSource>> = BTreeMap::new();
        let mut notes: Vec<String> = Vec::new();
        let walks: [(PathBuf, &'static str, &[&str]); 2] = [
            (self.addon_game_models_dir(), "addon-game", &[".vmdl_c"]),
            (self.addon_content_models_dir(), "addon-content", &[".vmdl"]),
        ];
        for (dir, label, extensions) in walks {
            // A missing addon directory is a note, not a silent empty success; its walk is
            // skipped (a symlinked models directory is "missing", never followed).
            if std::fs::symlink_metadata(&dir).map_or(true, |md| !md.is_dir()) {
                notes.push(format!(
                    "the {label} directory {} does not exist (no addon models scanned)",
                    dir.display()
                ));
                continue;
            }
            for relative in walk_models(&dir, extensions, &mut notes) {
                // The canonical spelling (the `model` keyvalue form): `models/` + lowercase,
                // `.vmdl` even when the file is `.vmdl_c`; paths are relative to the addon's
                // `models` root.
                let canonical = canonical_of_relative(&relative);
                let source = if label == "addon-game" {
                    ModelSource::AddonGame { file: PathBuf::from(&relative) }
                } else {
                    ModelSource::AddonContent { file: PathBuf::from(&relative) }
                };
                found.entry(canonical).or_default().push(source);
            }
        }
        (found, notes)
    }

    /// The sources list of a canonical path, creating the entry on first sight.
    fn sources_of(&mut self, canonical: &str) -> &mut Vec<ModelSource> {
        &mut self
            .entries
            .entry(canonical.to_string())
            .or_insert_with(|| ModelEntry {
                path: canonical.to_string(),
                sources: Vec::new(),
                compiled: false,
            })
            .sources
    }

    /// `<app>/game/csgo_addons/<addon>/models` — the addon's compiled models.
    fn addon_game_models_dir(&self) -> PathBuf {
        self.app_root
            .join("game/csgo_addons")
            .join(&self.addon)
            .join("models")
    }

    /// `<app>/content/csgo_addons/<addon>/models` — the addon's `.vmdl` sources.
    fn addon_content_models_dir(&self) -> PathBuf {
        self.app_root
            .join("content/csgo_addons")
            .join(&self.addon)
            .join("models")
    }

    /// Which game VPK holds `path` (any resource, e.g. `particles/x.vpcf_c`; case-insensitive),
    /// `Some("csgo")` / `Some("core")`: the trees the catalog already parsed, no re-parse.
    pub fn game_has(&self, path: &str) -> Option<&'static str> {
        if self.csgo.contains(path) {
            Some("csgo")
        } else if self.core.contains(path) {
            Some("core")
        } else {
            None
        }
    }

    /// The number of models in the catalog.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when the catalog holds no models at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, sorted by canonical path.
    pub fn entries(&self) -> impl Iterator<Item = &ModelEntry> {
        self.entries.values()
    }

    /// The scan's notes: what is *not* in the catalog and why (a missing addon directory, a
    /// walk that stopped at a bound). `None` when the scan was complete.
    pub fn note(&self) -> Option<String> {
        if self.notes.is_empty() {
            None
        } else {
            Some(self.notes.join("; "))
        }
    }

    /// One entry by any spelling the caller has: canonicalized with [`normalize_model_path`]
    /// (case, backslashes, the `.vmdl_c` form all spell one model).
    pub fn get(&self, model: &str) -> Option<&ModelEntry> {
        let canonical = normalize_model_path(model).ok()?;
        self.entries.get(&canonical)
    }

    /// Is the cached catalog still current? Only the csgo VPK's mtime moves the game part; a
    /// lookup miss rescans the addon dirs on its own.
    pub fn fresh(&self) -> bool {
        self.csgo_mtime == file_mtime(&self.app_root.join("game/csgo/pak01_dir.vpk"))
    }

    /// A miss-rescan: the same VPK trees (no re-parse), the addon walks run again, the mtime
    /// taken fresh — new addon files become visible through the returned catalog.
    pub fn rescan_addon(&self) -> ModelCatalog {
        Self::assemble(
            &self.app_root,
            &self.addon,
            Arc::clone(&self.csgo),
            Arc::clone(&self.core),
            file_mtime(&self.app_root.join("game/csgo/pak01_dir.vpk")),
        )
    }

    /// Up to `limit` cataloged models that could be what a lost path meant: the same directory
    /// first (the catalog map is sorted by canonical path), then entries whose *file stem*
    /// shares a case-insensitive substring with the requested one (the extension is not part of
    /// a name; both stems must be at least 3 characters — review w2b #10), those ranked by
    /// shared-prefix length before the alphabetical tiebreak. The request itself is never a
    /// suggestion. The list is bounded (`5` at the callers); `ModelNotFound` data carries it.
    pub fn suggestions(&self, model: &str, limit: usize) -> Vec<String> {
        let canonical = normalize_model_path(model).unwrap_or_else(|_| model.trim().to_lowercase());
        let (parent, stem) = match canonical.rsplit_once('/') {
            Some((head, tail)) => (head.to_string(), tail.to_string()),
            None => (String::new(), canonical.clone()),
        };
        // the file name without the extension (`chair_missing.vmdl` -> `chair_missing`)
        let stem = {
            let trimmed = stem
                .strip_suffix(".vmdl_c")
                .unwrap_or(&stem);
            trimmed.strip_suffix(".vmdl").unwrap_or(trimmed).to_string()
        };
        let stem_of = |path: &str| -> String {
            let name = path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path);
            let trimmed = name.strip_suffix(".vmdl_c").unwrap_or(name);
            trimmed.strip_suffix(".vmdl").unwrap_or(trimmed).to_string()
        };
        // review w2b #10: shares needs a real name match on BOTH spellings — an entry stem like
        // "ch" or "a" is a substring of most requests, so short stems filled the five slots for
        // every cross-directory miss. Cross-directory candidates are ranked by a shared-prefix
        // length over the stems (closeness) before the alphabetical tiebreak; same-directory
        // entries keep the catalog's sorted order and outrank them.
        let shared_prefix = |a: &str, b: &str| -> usize {
            a.as_bytes()
                .iter()
                .zip(b.as_bytes())
                .take_while(|(a, b)| a == b)
                .count()
        };
        let lowered_stem = stem.to_ascii_lowercase(); // computed once for the prefix ranking
        let mut same_dir: Vec<&String> = Vec::new();
        let mut near: Vec<(usize, &String)> = Vec::new();
        for path in self.entries.keys() {
            if path.to_ascii_lowercase() == canonical {
                continue; // the request itself is never a suggestion
            }
            let in_same_dir = match path.rsplit_once('/') {
                Some((head, _)) => head.eq_ignore_ascii_case(&parent),
                None => parent.is_empty(),
            };
            let entry_stem = stem_of(path);
            let lowered_entry = entry_stem.to_ascii_lowercase();
            let shares = !stem.is_empty()
                && stem.len() >= 3
                && entry_stem.len() >= 3
                && (lowered_entry.contains(&stem) || lowered_stem.contains(&lowered_entry));
            if in_same_dir {
                same_dir.push(path);
            } else if shares {
                near.push((shared_prefix(&lowered_stem, &lowered_entry), path));
            }
        }
        same_dir.sort();
        near.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let mut scored: Vec<&String> = same_dir
            .into_iter()
            .chain(near.into_iter().map(|(_, path)| path))
            .collect();
        scored.truncate(limit);
        scored.into_iter().cloned().collect()
    }

    /// The model's compiled bytes: the addon game file first (highest precedence), then the
    /// VPK routes in order. Refusals are explicit strings — `"no such model"` for an unknown
    /// path and `"no compiled .vmdl_c"` for a cataloged-but-uncompiled one. A source that
    /// existed at scan time but no longer reads falls through to the next one, and the failure
    /// of every route is the error.
    pub fn read_compiled(&self, model: &str) -> Result<Vec<u8>, String> {
        let canonical = normalize_model_path(model)?;
        let entry = match self.entries.get(&canonical) {
            Some(entry) => entry,
            None => return Err(format!("no such model: {canonical}")),
        };
        if !entry.compiled {
            return Err(format!(
                "{canonical}: no compiled .vmdl_c for the cataloged sources {} (not compiled yet)",
                entry
                    .sources
                    .iter()
                    .map(ModelSource::label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut failures: Vec<String> = Vec::new();
        for source in &entry.sources {
            match source {
                ModelSource::AddonGame { file } => {
                    // The path the walk saw (its original case), joined onto the addon's
                    // `models` root: never rebuilt from the lowercased canonical.
                    let path = self.addon_game_models_dir().join(file);
                    match std::fs::read(&path) {
                        Ok(bytes) => return Ok(bytes),
                        Err(error) => failures.push(format!("{}: {error}", path.display())),
                    }
                }
                ModelSource::Vpk { pak } => {
                    let vpk = if *pak == "csgo" {
                        &self.csgo
                    } else {
                        &self.core
                    };
                    let path = format!("{canonical}_c");
                    match vpk.read(&path) {
                        Ok(bytes) => return Ok(bytes),
                        Err(error) => failures.push(format!("pak {pak}: {error}")),
                    }
                }
                ModelSource::AddonContent { .. } => {} // a source file is never the compiled bytes
            }
        }
        Err(format!(
            "{canonical}: no compiled .vmdl_c could be read ({})",
            failures.join("; ")
        ))
    }
}

/// A file's mtime, or `None` when it cannot be read (and so counts as moved: never fresh).
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|md| md.modified().ok())
}

/// The canonical name of one walked file: its `models/…` spine lowercased, `.vmdl` even when
/// the file is `.vmdl_c` — the `model` keyvalue spelling. The canonical is only ever matched
/// against and matched by other canonical spellings (which are lowercase too).
fn canonical_of_relative(relative: &str) -> String {
    let lowered = format!("models/{relative}").to_lowercase();
    match lowered.strip_suffix(".vmdl_c") {
        Some(stem) => format!("{stem}.vmdl"),
        None => lowered,
    }
}

/// `name` ends with `ext` (ASCII case-insensitively): Wine/Windows-side tooling writes mixed
/// case (`Foo.VMDL_C`), and every other spelling rule in the module is case-insensitive too.
fn has_extension(name: &str, ext: &str) -> bool {
    name.len() >= ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

// -------------------------------------------------------------------------------------------
// the addon walks: bounded depth and count, symlinks never followed
// -------------------------------------------------------------------------------------------

/// The files under `dir` (a `models` root) whose name ends with one of `extensions`
/// (case-insensitively), relative to `dir`, in no order (the catalog's map sorts). Not entering
/// directories past [`WALK_MAX_DEPTH`], stopping after [`WALK_MAX_FILES`] files, never following
/// symlinks. Unreadable directories are noted; every bound hit is noted too (never a silent
/// miss).
pub(crate) fn walk_models(dir: &Path, extensions: &[&str], notes: &mut Vec<String>) -> Vec<String> {
    walk_files_bounded(dir, extensions, "model", notes)
}

/// [`walk_models`] for any asset kind: `what` names the kind in the notes (`"model"`,
/// `"material"`, …). The file bound counts every file the walk visits, not only the matches, so a
/// walk over a tree of other files (textures are found under the whole addon) stays bounded too.
pub(crate) fn walk_files_bounded(
    dir: &Path,
    extensions: &[&str],
    what: &str,
    notes: &mut Vec<String>,
) -> Vec<String> {
    let mut relative: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut depth_bound_hit = false;
    let mut count_bound_hit = false;
    // Depth-first, `depth` = the number of directory components under the `models` root (0).
    let mut stack: Vec<(String, PathBuf, usize)> = vec![(String::new(), dir.to_path_buf(), 0)];
    while let Some((prefix, here, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&here) {
            Ok(entries) => entries,
            Err(error) => {
                notes.push(format!("{} could not be read: {error}", here.display()));
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            // `file_type` reports symlinks as such: never followed, never scanned.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue; // a symlink to anywhere is never followed, never scanned
            }
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            if kind.is_dir() {
                if depth >= WALK_MAX_DEPTH {
                    depth_bound_hit = true;
                    continue; // deeper directories are not entered
                }
                stack.push((format!("{prefix}{name}/"), here.join(&name), depth + 1));
            } else if kind.is_file() {
                count += 1;
                if count > WALK_MAX_FILES {
                    count_bound_hit = true;
                    break; // the walk stops, never an unbounded scan
                }
                if extensions.iter().any(|ext| has_extension(&name, ext)) {
                    relative.push(format!("{prefix}{name}"));
                }
            }
        }
        if count_bound_hit {
            break;
        }
    }
    if depth_bound_hit {
        notes.push(format!(
            "the addon {what} walk stopped at the {WALK_MAX_DEPTH}-directory bound \
             and skipped the deeper directories and files of {}",
            dir.display()
        ));
    }
    if count_bound_hit {
        notes.push(format!(
            "the addon {what} walk stopped at its {WALK_MAX_FILES}-file bound"
        ));
    }
    relative.sort();
    relative.dedup();
    relative
}

// -------------------------------------------------------------------------------------------
// canonical paths
// -------------------------------------------------------------------------------------------

/// The canonical `model` keyvalue spelling of a model argument: lowercase, `/` separated,
/// `.vmdl` (the `.vmdl_c` form maps to the same file), always `models/**` or `agents/models/**`. Refusals: absolute
/// paths (a `/` root, a `C:` drive, anything with `:`), any `.`/`..`/empty segment, paths
/// outside `models/`, and extensions that are not `.vmdl`/`.vmdl_c`.
pub fn normalize_model_path(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("empty model path".to_string());
    }
    // Lowercase first: the rest validates (and is matched against the catalog) in canonical case.
    let path = text.replace('\\', "/").to_lowercase();
    if path.contains(':') || path.starts_with('/') {
        return Err(format!("absolute model path not allowed: {text}"));
    }
    let segments: Vec<&str> = path.split('/').collect();
    for segment in &segments {
        if segment.is_empty() {
            return Err(format!("empty segment in model path: {text}"));
        }
        if *segment == "." || *segment == ".." {
            return Err(format!(".. in model path not allowed: {text}"));
        }
    }
    // `agents/models/**` holds the real player agents (the `characters/models` ones are stubs)
    let root = if segments.first() == Some(&"agents") && segments.get(1) == Some(&"models") { 2 } else { 1 };
    if (root == 1 && segments.first() != Some(&"models")) || segments.len() < root + 1 {
        return Err(format!("path is not under models/ or agents/models/: {text}"));
    }
    let canonical = match path.strip_suffix(".vmdl_c") {
        Some(stem) => format!("{stem}.vmdl"),
        None if path.ends_with(".vmdl") => path,
        None => {
            return Err(format!(
                "not a model path (expected .vmdl or .vmdl_c): {text}"
            ))
        }
    };
    Ok(canonical)
}

/// A `models/…` prefix for `list_models` (`"models/dev/"`), with `..`/absolute/empty and
/// non-`models/` spellings refused (as an argument-error string).
fn normalize_prefix(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("empty prefix".to_string());
    }
    // Lowercase first (the match against the catalog is case-insensitive); one trailing
    // separator is the canonical spelling and is kept on the result.
    let path = text.replace('\\', "/").to_lowercase();
    if path.contains(':') || path.starts_with('/') {
        return Err(format!("absolute prefix not allowed: {input}"));
    }
    let without_trailing = path.strip_suffix('/').unwrap_or(&path);
    for segment in without_trailing.split('/') {
        if segment.is_empty() {
            return Err(format!("empty segment in prefix: {input}"));
        }
        if segment == "." || segment == ".." {
            return Err(format!(".. in prefix not allowed: {input}"));
        }
    }
    let under = |root: &str| without_trailing == root || without_trailing.starts_with(&format!("{root}/"));
    if !under("models") && !under("agents/models") {
        return Err(format!("prefix is not under models/ or agents/models/: {input}"));
    }
    if path.ends_with('/') {
        Ok(path)
    } else {
        Ok(format!("{path}/"))
    }
}

// -------------------------------------------------------------------------------------------
// the state cache: lazy build, mtime refresh, miss rescan
// -------------------------------------------------------------------------------------------

/// The cached catalog for `state`: built inside `tokio::task::spawn_blocking` on a miss,
/// rebuilt when the csgo `pak01_dir.vpk` mtime has moved, and (on a lookup miss) replaced by an
/// addon-only rescan through [`ModelCatalog::rescan_addon`]. Errors: [`catalog_tool_error`].
pub async fn catalog_for(state: &HammerState) -> Result<Arc<ModelCatalog>, ToolError> {
    {
        let cached = state
            .model_catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(catalog) = cached.as_ref() {
            // keyed by addon: a catalog built for another addon is never served (WP1.1)
            if catalog.addon() == state.addon() && catalog.fresh() {
                return Ok(Arc::clone(catalog));
            }
        }
    }
    let app_root = state.content().app_root.clone();
    let addon = state.addon();
    let built = tokio::task::spawn_blocking(move || ModelCatalog::build(&app_root, &addon))
        .await
        .map_err(|error| ToolError::internal(format!("{PLAN}: the build task failed: {error}")))?
        .map_err(catalog_tool_error)?;
    let catalog = Arc::new(built);
    set_cached(state, Arc::clone(&catalog));
    Ok(catalog)
}

/// Store the (new) cached catalog: the happy-path value here.
fn set_cached(state: &HammerState, catalog: Arc<ModelCatalog>) {
    *state
        .model_catalog
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(catalog);
}

/// A failed build as a tool error: `VpkMissing` names the file (never an empty catalog), a
/// VPK parse failure is explicit too.
pub fn catalog_tool_error(error: CatalogError) -> ToolError {
    match error {
        CatalogError::MissingVpk { path } => ToolError::new(
            "VpkMissing",
            "VpkMissing",
            format!(
                "the model catalog cannot be built: the game's directory VPK {path} is missing"
            ),
        )
        .with_data(json!({"path": path}))
        .retry_safe(true),
        other => ToolError::new(
            "CatalogVpkError",
            "CatalogVpkError",
            format!("the model catalog cannot read the game VPKs: {other}"),
        )
        .retry_safe(true),
    }
}

/// One model in the catalog, by any spelling ([`normalize_model_path`]). On a miss the addon
/// directories are rescanned and the rescan stored — addon files appear during a session — and
/// a still-missing model is `Ok(None)`. Hook for the other prop tools (WP-M/WP-P).
pub async fn lookup_model(
    state: &HammerState,
    model: &str,
) -> Result<Option<ModelEntry>, ToolError> {
    let canonical = normalize_model_path(model)
        .map_err(|reason| ToolError::invalid_arguments(format!("bad model path: {reason}")))?;
    let catalog = catalog_for(state).await?;
    if let Some(entry) = catalog.get(&canonical) {
        return Ok(Some(entry.clone()));
    }
    // Miss: addon files change during a session — rescan only the addon dirs (the VPK trees
    // are reused) and store the rescan as the catalog, then retry once.
    let rescanned = tokio::task::spawn_blocking(move || catalog.rescan_addon())
        .await
        .map_err(|error| ToolError::internal(format!("{PLAN}: the rescan task failed: {error}")))?;
    let rescanned = Arc::new(rescanned);
    set_cached(state, Arc::clone(&rescanned));
    Ok(rescanned.get(&canonical).cloned())
}

/// The compiled bytes of one cataloged model (`read_compiled`'s tool-side hook). A model the
/// catalog does not know is `ModelNotFound`, a known-but-uncompiled one `ModelNotCompiled`.
pub async fn read_compiled_model(state: &HammerState, model: &str) -> Result<Vec<u8>, ToolError> {
    let canonical = normalize_model_path(model)
        .map_err(|reason| ToolError::invalid_arguments(format!("bad model path: {reason}")))?;
    match lookup_model(state, model).await? {
        None => Err(not_found(&canonical)),
        Some(entry) if entry.compiled => {
            let catalog = catalog_for(state).await?;
            catalog
                .read_compiled(&canonical)
                .map_err(|message| read_error(&canonical, message))
        }
        Some(_) => Err(ToolError::new(
            "ModelNotCompiled",
            "ModelNotCompiled",
            format!("the model {canonical} has no compiled .vmdl_c (the addon's source is not compiled yet)"),
        )
        .with_data(json!({"model": canonical}))
        .retry_safe(true)),
    }
}

/// `ModelNotFound` naming the canonical model (the rescan already saw that it is not there).
fn not_found(canonical: &str) -> ToolError {
    ToolError::new(
        "ModelNotFound",
        "ModelNotFound",
        format!("no model named {canonical} in the model catalog"),
    )
    .with_data(json!({"model": canonical}))
    .retry_safe(true)
}

/// A read that failed after the lookup hit: the source disappeared mid-read or is unreadable.
fn read_error(canonical: &str, message: String) -> ToolError {
    if message.contains("no such model") {
        not_found(canonical)
    } else if message.contains("no compiled .vmdl_c") {
        ToolError::new(
            "ModelNotCompiled",
            "ModelNotCompiled",
            format!("the model {canonical} has no compiled .vmdl_c to read"),
        )
        .with_data(json!({"model": canonical}))
        .retry_safe(true)
    } else {
        ToolError::new("ModelRead", "ModelRead", format!("{canonical}: {message}")).retry_safe(true)
    }
}

// -------------------------------------------------------------------------------------------
// the tool
// -------------------------------------------------------------------------------------------

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "list_models",
            "List .vmdl models in the game VPKs and the current addon. Use an entry's `model` value verbatim as a prop's `model` keyvalue." ,
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "case-insensitive substring; '*' or '?' makes it a glob"
                },
                "prefix": {
                    "type": "string",
                    "description": "lowercase path prefix, e.g. 'models/props/'"
                },
                "source": {
                    "type": "string",
                    "enum": ["any", "addon", "game"]
                },
                "compiledOnly": {
                    "type": "boolean"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": LIMIT_MAX
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0
                }
            },
            "additionalProperties": false
        })),
        list_models,
    );
}

/// `limit?: integer` — [`LIMIT_DEFAULT`] when absent, 1..=[`LIMIT_MAX`] when given.
fn limit_arg(args: &Value) -> Result<u64, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(limit),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
        ))),
    }
}

/// `offset?: integer` >= 0.
fn offset_arg(args: &Value) -> Result<u64, ToolError> {
    match args.get("offset") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(offset) => Ok(offset),
            None => Err(ToolError::invalid_arguments(format!(
                "offset must be a non-negative integer, got {n}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "offset must be a non-negative integer, got {other}"
        ))),
    }
}

/// Optional string argument: `None` when absent or null, an error for any other type.
fn optional_str<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// Optional boolean argument: `None` when absent or null, an error for any other type.
fn optional_bool(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// The known arguments of `list_models`; anything else is refused before any dispatch.
const KNOWN_ARGS: &[&str] = &[
    "query",
    "prefix",
    "source",
    "compiledOnly",
    "limit",
    "offset",
];

/// Every model matching the filters, sorted by canonical path and paged. The result's `model`
/// values are the prop `model` keyvalues; `sources` labels (`addon-game`, `addon-content`,
/// `vpk:csgo`, `vpk:core`) tell where each file lives, `note` (when present) what the scan
/// could not see. A missing game VPK is an error naming the file: never a silent empty list.
pub async fn list_models(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let map = match &args {
        Value::Object(map) => map,
        other => {
            return Err(ToolError::invalid_arguments(format!(
                "arguments must be an object, got {other}"
            )))
        }
    };
    for key in map.keys() {
        if !KNOWN_ARGS.contains(&key.as_str()) {
            return Err(ToolError::invalid_arguments(format!(
                "unknown argument {key:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    let query = optional_str(&args, "query")?;
    if query == Some("") {
        return Err(ToolError::invalid_arguments(
            "query is empty (omit it or give a substring/glob)",
        ));
    }
    let prefix = match optional_str(&args, "prefix")? {
        None => None,
        Some(text) => Some(normalize_prefix(text).map_err(ToolError::invalid_arguments)?),
    };
    let source = match optional_str(&args, "source")? {
        None | Some("any") => Source::Any,
        Some("addon") => Source::Addon,
        Some("game") => Source::Game,
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "source must be \"any\", \"addon\" or \"game\", got {other:?}"
            )))
        }
    };
    let compiled_only = optional_bool(&args, "compiledOnly", false)?;
    let limit = limit_arg(&args)? as usize;
    let offset = offset_arg(&args)? as usize;
    let addon = state.addon();
    let catalog = catalog_for(&state).await?;
    // Lowercased once for every entry (13k allocations per call otherwise); the glob check
    // rides on the lowered text (`*`/`?` are the same characters).
    let query_lowered = query.map(|q| {
        let glob = q.contains('*') || q.contains('?');
        (q.to_lowercase(), glob)
    });

    let matches: Vec<&ModelEntry> = catalog
        .entries()
        .filter(|entry| source.keeps(entry))
        .filter(|entry| !compiled_only || entry.compiled)
        .filter(|entry| match &prefix {
            None => true,
            Some(prefix) => entry.path.starts_with(prefix),
        })
        .filter(|entry| match &query_lowered {
            None => true,
            Some((lowered, glob)) => {
                if *glob {
                    glob_match(lowered, &entry.path)
                } else {
                    entry.path.contains(lowered.as_str())
                }
            }
        })
        .collect();
    let total = matches.len();
    let page: Vec<Value> = matches
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|entry| {
            json!({
                "model": entry.path,
                "sources": entry.sources.iter().map(ModelSource::label).collect::<Vec<_>>(),
                "compiled": entry.compiled,
            })
        })
        .collect();
    let returned = page.len();

    let mut out = json!({
        "total": total,
        "offset": offset,
        "returned": returned,
        "truncated": offset + returned < total,
        "models": page,
        "addon": addon,
    });
    if let Some(note) = catalog.note() {
        out.as_object_mut()
            .expect("json object")
            .insert("note".into(), json!(note));
    }
    Ok(out.into())
}

/// Which routes an entry's `sources` should carry for the `source` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Any,
    Addon,
    Game,
}

impl Source {
    fn keeps(&self, entry: &ModelEntry) -> bool {
        match self {
            Source::Any => true,
            Source::Addon => entry.sources.iter().any(|s| {
                matches!(
                    s,
                    ModelSource::AddonGame { .. } | ModelSource::AddonContent { .. }
                )
            }),
            Source::Game => entry
                .sources
                .iter()
                .any(|s| matches!(s, ModelSource::Vpk { .. })),
        }
    }
}

/// A finite bounded glob (`*` any run, `?` one char; everything else literal), DP over the
/// pattern/text — never a backtracking blow-up on a pathological pattern.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    // row[j]: pattern[..i] matches text[..j]; after processing all of `p`, row[n] is the answer.
    let mut row = vec![false; n + 1];
    row[0] = true;
    for &pc in p.iter() {
        let mut next = vec![false; n + 1];
        for j in 0..=n {
            next[j] = match pc {
                '*' => row[j] || (j > 0 && next[j - 1]),
                '?' => j > 0 && row[j - 1],
                c => j > 0 && row[j - 1] && c == t[j - 1],
            };
        }
        row = next;
    }
    row[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// normalize_model_path: canonical happy paths and every documented refusal.
    #[test]
    fn normalize_canonicalizes_or_refuses() {
        assert_eq!(
            normalize_model_path("models/props/box.vmdl_c"),
            Ok("models/props/box.vmdl".to_string())
        );
        assert_eq!(
            normalize_model_path("  models\\props\\Box.VMDL  "),
            Ok("models/props/box.vmdl".to_string())
        );
        assert_eq!(
            normalize_model_path("models/props/box.vmdl"),
            Ok("models/props/box.vmdl".to_string())
        );
        for bad in [
            "",
            "   ",
            "models/x.vmat",
            "materials/steel.vmat",
            "C:/x.vmdl",
            "/models/box.vmdl",
            "models/../box.vmdl",
            "models/./box.vmdl",
            "models",
            "models/",
            "props/box.vmdl",
            "models/props/box.exe",
        ] {
            assert!(normalize_model_path(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// prefix canonicalization: lowercase + backslashes, `models/` root enforced, and the
    /// canonical result always ends in `/` (a `starts_with` on the entry paths).
    #[test]
    fn prefix_rules() {
        assert_eq!(
            normalize_prefix("MODELS\\dev\\"),
            Ok("models/dev/".to_string())
        );
        assert_eq!(normalize_prefix("models"), Ok("models/".to_string()));
        for bad in [
            "",
            "props/",
            "materials/",
            "C:/models/",
            "models/../x",
            "models/a//b",
            "models//",
        ] {
            assert!(normalize_prefix(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// The DP glob: '*' is any run, '?' is one char, literals otherwise; a pathological
    /// `*`-heavy pattern stays bounded (no exponential backtracking).
    #[test]
    fn the_glob_matches_and_stays_bounded() {
        assert!(glob_match(
            "models/props/*box*",
            "models/props/dust/box.vmdl"
        ));
        assert!(glob_match(
            "models/props/?h?ir.vmdl",
            "models/props/chair.vmdl"
        ));
        assert!(!glob_match("*box*", "models/props/dust/pipes.vmdl"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        // 50 stars vs 100 `a`s: a naive backtracker explodes here, the DP answers instantly.
        let stars_only = "*a".repeat(49) + "b";
        assert!(!glob_match(&stars_only, &"a".repeat(100)));
        assert!(glob_match("*a*a*b", "aaaaab"));
    }
}
