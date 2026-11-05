//! The soundevent name index (WP-S1): every `.vsndevts_c` / `.vsndevts` the game and the active
//! addon carry, one entry per event name, and the two gates the sound tools share
//! ([`index_for`], [`check_sound_name`]).
//!
//! Sources in precedence order (one entry per event name, `sources` lists every holder, best
//! first — an addon copy outranks the game, exactly like `tools/model_catalog.rs`):
//!
//! * `addon-game` — `game/csgo_addons/<addon>/soundevents/**/*.vsndevts_c` (what the runtime
//!   loads: the compiled truth);
//! * `addon-source` — `content/csgo_addons/<addon>/soundevents/**/*.vsndevts` (KV3 text through
//!   `cs2_kv3::parse`; a name here but not in `addon-game` is `compiled:false`);
//! * `vpk:csgo` — `soundevents/**/*.vsndevts_c` in `game/csgo/pak01_dir.vpk`;
//! * `vpk:core` — the same in `game/core/pak01_dir.vpk` (the `amb.*.base` templates ship in
//!   `soundevents/soundevents_soundscapes_core.vsndevts_c` here — the `#[ignore]`d corpus test
//!   records which file the index found them in).
//!
//! A compiled block is a resource container (`crate::resource::Resource`) whose `DATA` block is
//! binary KV3; the text source is parsed by `cs2-kv3`. Both become the same
//! `properties` JSON object (`Kv3Value` -> JSON with flagged values unwrapped), so every reader
//! below works on one shape. The facts the tools need are read out of it
//! ([`SoundEventEntry::type_`]/`base`/`volume`/`vsnds`/`children`/`position`).
//!
//! Nothing is ever silently empty: a missing game VPK is [`IndexError::VpkMissing`], an
//! unreadable one [`IndexError::VpkUnreadable`] (both name the file, `tools::soundevents_query`
//! maps them to `VpkMissing`/`CatalogVpkError`), while an absent addon directory, a corrupt
//! `.vsndevts_c` and a `.vsndevts` text that does not parse are `notes` entries — the index keeps
//! answering from the sources that did read (a corrupt addon file must not hide the game's
//! 21.6k events).
//!
//! Caching: [`index_for`] builds once inside `tokio::task::spawn_blocking` and caches the result
//! on [`HammerState::soundevent_index`]; [`SoundEventIndex::fresh`] re-`stat`s the two game VPKs,
//! the two addon `soundevents` directories and every addon soundevent file it read, so a write or
//! a compile is noticed on the next call. [`SoundEventIndex::rescan_addon`] re-walks the addon
//! only (the two VPK trees and their decoded events are reused: no re-open, no re-decode) and is
//! what a caller uses when it knows a file appeared. The game half is held as a shared
//! `GamePart` behind an `Arc` (review N10: a rescan must not deep-clone the ~21.8k decoded
//! property objects) and is **copied** into every assembled index (review N1: a `mem::take` used
//! to gut it, so a rescanned index lost the VPK watches, the game sound assets and the VPK notes).
//! `HammerState::soundevent_index_invalidate`
//! (called by the file writers) drops the cache outright, and [`index_for_report`] takes the
//! dropped index (`HammerState::soundevent_index_last`) and **rescans the addon** whenever both
//! paks are untouched ([`SoundEventIndex::vpk_unchanged`]) — a write or a compile costs one addon
//! walk, never a re-decode of the 193 corpus blocks (review M3). Plan: `target/orch/plan-snd.md`
//! §1.
//! Tests: `tests/soundevents_index.rs` (+ the `list_soundevents` tool tests in
//! `tests/soundevents_query.rs`).
use crate::resource::{decode_kv3, Kv3Value, Resource};
use crate::state::HammerState;
use crate::vpk::VpkDir;
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

/// Where this module's rules come from (error messages name it).
const PLAN: &str = "target/orch/plan-snd.md §1 (WP-S1)";
/// The addon walk's directory bound; deeper directories are not entered and the scan notes it.
const WALK_MAX_DEPTH: usize = 8;
/// The addon walk's file bound per directory tree.
const WALK_MAX_FILES: usize = 20_000;
/// The `sounds/` asset path set the `vsnds[].known` check runs over is capped here (a name past
/// the cap is simply not recorded; the set only ever grows with the addon's own files).
const SOUND_FILES_MAX: usize = 200_000;
/// `resolve_chain` stops after this many events (16 hops) and reports `cycle:false` — a
/// `base` ring that never repeats a name exactly (impossible with names, but cheap to bound).
const CHAIN_MAX: usize = 16;

/// Where one copy of an event lives. The derived `Ord` is the precedence order (best first), so
/// `sort_by_key` on the enum sorts an entry's copies correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SoundSource {
    AddonGame,
    AddonSource,
    VpkCsgo,
    VpkCore,
}

impl SoundSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SoundSource::AddonGame => "addon-game",
            SoundSource::AddonSource => "addon-source",
            SoundSource::VpkCsgo => "vpk:csgo",
            SoundSource::VpkCore => "vpk:core",
        }
    }
    /// `any` matches all; `addon` the two addon sources; `game` the two VPKs.
    pub fn is_addon(self) -> bool {
        matches!(self, SoundSource::AddonGame | SoundSource::AddonSource)
    }
    /// `true` when a copy in this source is a *compiled* one (`addon-game` or a VPK).
    fn is_compiled(self) -> bool {
        !matches!(self, SoundSource::AddonSource)
    }
}

/// One soundevent (the top-ranked copy's facts; `sources` lists every holder).
#[derive(Debug, Clone, PartialEq)]
pub struct SoundEventEntry {
    pub name: String,
    /// The holding file of the top-ranked copy. VPK copies carry the VPK-internal path
    /// (`soundevents/ambience/game_sounds_dust2.vsndevts_c`; `sources` names which pak), addon
    /// copies the path relative to the app root
    /// (`game/csgo_addons/<addon>/soundevents/soundevents_addon.vsndevts_c`).
    pub file: String,
    /// Every source carrying the name, best first.
    pub sources: Vec<SoundSource>,
    /// True when a compiled copy (`addon-game` or a VPK) exists.
    pub compiled: bool,
    pub type_: Option<String>,
    pub base: Option<String>,
    pub volume: Option<f64>,
    /// `vsnd_files_track_01` + `vsnd_files`, string-or-array normalised.
    pub vsnds: Vec<String>,
    /// `soundevent_01`, string-or-array normalised.
    pub children: Vec<String>,
    pub position: Option<[f64; 3]>,
    /// The full property object as JSON (typed arrays as arrays, flagged values unwrapped).
    pub properties: Value,
}

impl SoundEventEntry {
    /// The top-ranked source (an entry always has at least one copy).
    pub fn source(&self) -> SoundSource {
        self.sources
            .first()
            .copied()
            .unwrap_or(SoundSource::AddonSource)
    }
}

/// `resolve_chain`'s answer: base-first chain, the merged properties (child overrides base),
/// and whether a cycle was cut.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEvent {
    pub chain: Vec<String>,
    pub merged: Map<String, Value>,
    pub cycle: bool,
}

/// Why an index could not be built.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexError {
    /// A game VPK the index needs is absent/unreadable.
    VpkMissing { path: PathBuf, detail: String },
    /// A game VPK is there but cannot be parsed (`CatalogVpkError` at the tool boundary).
    VpkUnreadable { path: PathBuf, detail: String },
}

/// The verdict on one soundevent-typed keyvalue.
#[derive(Debug, Clone, PartialEq)]
pub enum SoundNameVerdict {
    /// Present in a compiled source.
    Known {
        name: String,
        source: SoundSource,
        file: String,
    },
    /// Only in the addon `.vsndevts` text: not compiled yet.
    SourceOnly { file: String },
    /// The name's **only** copy is the addon's compiled `_c`, while the `.vsndevts` source paired
    /// with that `_c` exists and no longer carries the name: it was renamed or removed in the
    /// source and the `_c` was never recompiled (review M1). The stale `_c` still answers a lookup
    /// today, but the map reload after the next compile plays nothing — a warning, never a
    /// refusal (the value may well be the name the user is about to add back).
    Stale { file: String },
    /// A path (`/` or a `.vsnd`/`.wav`/`.mp3` suffix): `ambient_generic.message`'s file form, not looked up.
    FilePath,
    /// `""`.
    Empty,
    /// Not found anywhere.
    Unknown { suggestions: Vec<String> },
}

// -------------------------------------------------------------------------------------------
// the reads: one copy of one event as it was found in one file
// -------------------------------------------------------------------------------------------

/// One event found in one file, before the merge.
#[derive(Debug, Clone)]
struct Copy {
    name: String,
    source: SoundSource,
    file: String,
    properties: Map<String, Value>,
}

/// The game-VPK half of a build, kept (behind an `Arc`, so every rescan shares it without a deep
/// clone) so [`SoundEventIndex::rescan_addon`] can reuse it: the VPKs are never re-opened and
/// never re-decoded, and — review N1 — the half is never gutted either, because `assemble` copies
/// out of it.
#[derive(Debug, Clone, Default)]
struct GamePart {
    copies: Vec<Copy>,
    notes: Vec<String>,
    /// Files whose `DATA` block decoded (the `files` count).
    files: usize,
    /// The `(path, mtime)` pairs `fresh()` re-checks (the two VPKs).
    watched: Vec<(PathBuf, Option<SystemTime>)>,
    /// Lowercased `sounds/**` asset paths (`.vsnd`/`.vsnd_c`/`.wav`/`.mp3`).
    sound_files: BTreeSet<String>,
    /// Scalar root members that are not events (`_auto_calc_vnsd_duration_`, …): reported once.
    non_events: BTreeSet<String>,
}

/// One `.vsndevts_c` block -> its events. A file that is not a resource, has no `DATA` block or
/// whose KV3 payload is corrupt is a `note`, never a failure (the rest of the index stands).
fn decode_compiled(
    bytes: &[u8],
    source: SoundSource,
    file: &str,
    copies: &mut Vec<Copy>,
    notes: &mut Vec<String>,
    files: &mut usize,
    non_events: &mut BTreeSet<String>,
) {
    let resource = match Resource::parse(bytes) {
        Ok(resource) => resource,
        Err(error) => {
            notes.push(format!("{file}: not a compiled resource: {error}"));
            return;
        }
    };
    let Some(block) = resource.blocks_of(b"DATA").next() else {
        notes.push(format!(
            "{file}: no DATA block (blocks: {})",
            resource.block_types().join(", ")
        ));
        return;
    };
    let document = match decode_kv3(resource.block_bytes(block)) {
        Ok(document) => document,
        Err(error) => {
            notes.push(format!("{file}: the DATA block does not decode: {error}"));
            return;
        }
    };
    let Some(members) = document.root.as_object() else {
        notes.push(format!(
            "{file}: the DATA root is a {}, not an event object",
            document.root.kind_name()
        ));
        return;
    };
    *files += 1;
    for (name, value) in members {
        let Value::Object(properties) = kv3_to_json(value) else {
            // The compiler writes a few scalar root members next to the events
            // (`_auto_calc_vnsd_duration_` is in almost every block); they are not events and
            // are reported once per build, not once per file (see `assemble`).
            non_events.insert(name.clone());
            continue;
        };
        copies.push(Copy {
            name: name.clone(),
            source,
            file: file.to_string(),
            properties,
        });
    }
}

/// One `.vsndevts` text -> its events. Returns `true` when the text decoded (the caller records
/// the file as a *parsed* source); a parse failure is a `note` naming line and column and the
/// file is left unparsed (the list still answers from the other sources), a non-UTF-8 file is a
/// note too. An unparsed source is no evidence either way for the addon `_c`'s staleness
/// (review N3): the caller keeps it out of the stale rule, so its names stay `Known`.
fn decode_text(
    text: &str,
    source: SoundSource,
    file: &str,
    copies: &mut Vec<Copy>,
    notes: &mut Vec<String>,
    files: &mut usize,
    non_events: &mut BTreeSet<String>,
) -> bool {
    let document = match cs2_kv3::parse(text) {
        Ok(document) => document,
        Err(error) => {
            notes.push(format!(
                "{file}: could not be parsed: line {}, column {}: {} (its names' staleness \
                 against the compiled file is unknown)",
                error.line, error.col, error.msg
            ));
            return false;
        }
    };
    *files += 1;
    for (name, value) in document.root.iter() {
        if value.as_object().is_none() {
            non_events.insert(name.to_string());
            continue;
        }
        let Value::Object(properties) = unwrap_flag_wrappers(cs2_kv3::to_json(value)) else {
            notes.push(format!("{file}: the event {name:?} is not a JSON object"));
            continue;
        };
        copies.push(Copy {
            name: name.to_string(),
            source,
            file: file.to_string(),
            properties,
        });
    }
    true
}

/// A binary `Kv3Value` as JSON. Flagged values are unwrapped (the plan's "flagged values
/// unwrapped"); a blob becomes `{"$blob": "<hex>"}` (a soundevent never carries one, but the
/// mapping must not lose bytes silently).
fn kv3_to_json(value: &Kv3Value) -> Value {
    match value {
        Kv3Value::Null => Value::Null,
        Kv3Value::Bool(value) => Value::Bool(*value),
        Kv3Value::Int(value) => Value::from(*value),
        Kv3Value::UInt(value) => Value::from(*value),
        Kv3Value::Double(value) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Kv3Value::String(text) => Value::String(text.clone()),
        Kv3Value::Blob(bytes) => json!({"$blob": hex_upper(bytes)}),
        Kv3Value::Array(items) | Kv3Value::TypedArray(items) => {
            Value::Array(items.iter().map(kv3_to_json).collect())
        }
        Kv3Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(name, member)| (name.clone(), kv3_to_json(member)))
                .collect(),
        ),
        Kv3Value::Flagged { value, .. } => kv3_to_json(value),
    }
}

/// `cs2_kv3::to_json` spells a flagged value `{"$flag": …, "value": …}`: unwrap exactly that
/// shape (an object merely *containing* `$flag` stays an object, the `from_json` rule mirrored).
fn unwrap_flag_wrappers(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            if map.len() == 2 && map.contains_key("$flag") && map.contains_key("value") {
                return unwrap_flag_wrappers(map.remove("value").expect("checked above"));
            }
            Value::Object(
                map.into_iter()
                    .map(|(key, member)| (key, unwrap_flag_wrappers(member)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(unwrap_flag_wrappers).collect()),
        other => other,
    }
}

/// Uppercase hex (`$blob` spelling, the `cs2_kv3` JSON convention).
fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

// -------------------------------------------------------------------------------------------
// read facts out of one event's properties
// -------------------------------------------------------------------------------------------

/// The facts a [`SoundEventEntry`] carries, read out of the JSON property object.
struct Facts {
    type_: Option<String>,
    base: Option<String>,
    volume: Option<f64>,
    vsnds: Vec<String>,
    children: Vec<String>,
    position: Option<[f64; 3]>,
}

impl Facts {
    fn of(properties: &Map<String, Value>) -> Facts {
        // string OR array of strings -> the list (anything else contributes nothing)
        let names = |key: &str| -> Vec<String> {
            match properties.get(key) {
                Some(Value::String(text)) => vec![text.clone()],
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            }
        };
        let mut vsnds = names("vsnd_files_track_01");
        vsnds.extend(names("vsnd_files"));
        for key in properties.keys() {
            if key.starts_with("vsnd_files_track_") && key != "vsnd_files_track_01" {
                vsnds.extend(names(key));
            }
        }
        Facts {
            type_: properties.get("type").and_then(Value::as_str).map(str::to_string),
            base: properties.get("base").and_then(Value::as_str).map(str::to_string),
            volume: properties.get("volume").and_then(Value::as_f64),
            vsnds,
            children: names("soundevent_01"),
            position: match properties.get("position").and_then(Value::as_array) {
                Some(items) if items.len() == 3 => {
                    let mut out = [0.0f64; 3];
                    let mut ok = true;
                    for (axis, item) in items.iter().enumerate() {
                        match item.as_f64() {
                            Some(number) => out[axis] = number,
                            None => ok = false,
                        }
                    }
                    ok.then_some(out)
                }
                _ => None,
            },
        }
    }
}

// -------------------------------------------------------------------------------------------
// the build
// -------------------------------------------------------------------------------------------

/// The cached index. `Default` is the empty index (the skeleton's shape); a real one comes from
/// [`SoundEventIndex::build`].
#[derive(Debug, Clone, Default)]
pub struct SoundEventIndex {
    pub(crate) entries: Vec<SoundEventEntry>,
    pub(crate) notes: Vec<String>,
    pub(crate) files: usize,
    pub(crate) built_ms: u64,
    /// The union of every indexed event's property keys (WP-S2's `unknownKeys` warning).
    known_keys: BTreeSet<String>,
    /// The app tree the index was built from (a rescan re-walks the addon under it).
    app_root: PathBuf,
    addon: String,
    /// The game-VPK half (shared with every rescan: `Arc`, review N1/N10).
    game: Arc<GamePart>,
    /// `(path, mtime)` pairs [`SoundEventIndex::fresh`] re-checks.
    watched: Vec<(PathBuf, Option<SystemTime>)>,
    /// The two game VPK `(path, mtime)` pairs alone: [`SoundEventIndex::vpk_unchanged`]'s gate for
    /// "rescan the addon instead of rebuilding" (review M3). Empty only on the `Default` stub,
    /// which therefore never counts as "the paks are untouched".
    vpk_watched: Vec<(PathBuf, Option<SystemTime>)>,
    /// The addon-source `.vsndevts` files found at build time, relative to the addon's
    /// `soundevents` directory: the stale rule needs to know whether a compiled file still has a
    /// source next to it.
    addon_source_files: BTreeSet<String>,
    /// The subset of [`SoundEventIndex::addon_source_files`] that did **not** decode (a syntax
    /// error or a non-UTF-8 read): such a file proves nothing about its compiled counterpart, so
    /// the stale rule skips it (review N3) — a name only the `_c` carries stays `Known`.
    addon_unparsed_files: BTreeSet<String>,
    /// Names whose only copy is the addon's `_c` whose paired `.vsndevts` source exists and does
    /// not carry them: name -> the source file they are missing from
    /// ([`SoundNameVerdict::Stale`], review M1).
    stale: BTreeMap<String, String>,
    /// Lowercased `sounds/**` asset paths from both paks and both addon trees.
    sound_files: BTreeSet<String>,
}

impl SoundEventIndex {
    /// The addon this index was built for.
    pub fn addon(&self) -> &str {
        &self.addon
    }

    /// Build from the game tree root (`<project>/workshop/run/app`) and the addon name. Both game
    /// VPKs are required (each missing one is [`IndexError::VpkMissing`]); the addon
    /// directories may legitimately not exist (that is a note, never silence).
    pub fn build(app_root: &Path, addon: &str) -> Result<Self, IndexError> {
        let started = Instant::now();
        let game = Arc::new(read_game(app_root)?);
        Ok(Self::assemble(app_root, addon, game, started))
    }

    /// The merged index from a shared game half plus a fresh addon walk.
    ///
    /// Review N1: the game half is **copied** out of `game` (watches, sound assets, notes, the
    /// non-event root members), never `mem::take`n — [`SoundEventIndex::rescan_addon`] feeds the
    /// same `GamePart` back in, so a take would gut it and every later rescan would carry no VPK
    /// watch (`fresh`/`vpk_unchanged` false), no game sound asset (`vsnds[].known` false) and no
    /// VPK notes. Review N10: `game` is an `Arc`, so the clone here is a pointer copy and the
    /// ~21.8k decoded property objects are never deep-cloned.
    fn assemble(
        app_root: &Path,
        addon: &str,
        game: Arc<GamePart>,
        started: Instant,
    ) -> SoundEventIndex {
        let mut index = SoundEventIndex {
            app_root: app_root.to_path_buf(),
            addon: addon.to_string(),
            watched: game.watched.clone(),
            vpk_watched: game.watched.clone(),
            sound_files: game.sound_files.clone(),
            notes: game.notes.clone(),
            files: game.files,
            ..SoundEventIndex::default()
        };
        let mut addon_copies: Vec<Copy> = Vec::new();
        let mut non_events: BTreeSet<String> = game.non_events.clone();

        // the addon's two soundevents trees, then its two sounds/ asset trees
        let addon_dirs: [(PathBuf, &'static str, &'static [&'static str]); 2] = [
            (
                app_root
                    .join("game/csgo_addons")
                    .join(addon)
                    .join("soundevents"),
                "game",
                &[".vsndevts_c"],
            ),
            (
                app_root
                    .join("content/csgo_addons")
                    .join(addon)
                    .join("soundevents"),
                "content",
                &[".vsndevts"],
            ),
        ];
        for (dir, which, extensions) in &addon_dirs {
            let (source, label) = if *which == "game" {
                (SoundSource::AddonGame, "addon-game")
            } else {
                (SoundSource::AddonSource, "addon-source")
            };
            // the directory's own mtime: a file appearing in it must move `fresh()` too
            index.watched.push((dir.clone(), file_mtime(dir)));
            if std::fs::symlink_metadata(dir).map_or(true, |md| !md.is_dir()) {
                index.notes.push(format!(
                    "the {label} directory {} does not exist (no addon soundevents scanned)",
                    dir.display()
                ));
                continue;
            }
            for relative in walk_files(dir, extensions, &mut index.notes) {
                let path = dir.join(&relative);
                index.watched.push((path.clone(), file_mtime(&path)));
                let shown = format!("{which}/csgo_addons/{addon}/soundevents/{relative}");
                if *which == "game" {
                    match std::fs::read(&path) {
                        Ok(bytes) => decode_compiled(
                            &bytes,
                            source,
                            &shown,
                            &mut addon_copies,
                            &mut index.notes,
                            &mut index.files,
                            &mut non_events,
                        ),
                        Err(error) => index.notes.push(format!("{shown}: {error}")),
                    }
                } else {
                    // The addon-source files present, by their path under `soundevents/`: M1's
                    // stale rule asks whether a compiled file still has its source next to it.
                    // Review N3: a file that does not decode is recorded as unparsed, so it can
                    // never make its `_c`'s names look stale (the failure says nothing about the
                    // `_c`).
                    let decoded = match std::fs::read_to_string(&path) {
                        Ok(text) => decode_text(
                            &text,
                            source,
                            &shown,
                            &mut addon_copies,
                            &mut index.notes,
                            &mut index.files,
                            &mut non_events,
                        ),
                        Err(error) => {
                            index.notes.push(format!(
                                "{shown}: could not be read as UTF-8 text (its names' staleness \
                                 against the compiled file is unknown): {error}"
                            ));
                            false
                        }
                    };
                    index.addon_source_files.insert(relative.clone());
                    if !decoded {
                        index.addon_unparsed_files.insert(relative.clone());
                    }
                }
            }
        }
        // The addon's own sound assets (the `vsnds[].known` check wants them). A missing tree is
        // not a note: the soundevents note above already says the addon may be absent, and an
        // addon without `sounds/` is normal.
        for tree in ["game/csgo_addons", "content/csgo_addons"] {
            let dir = app_root.join(tree).join(addon).join("sounds");
            if std::fs::symlink_metadata(&dir).map_or(true, |md| !md.is_dir()) {
                continue;
            }
            for relative in walk_sounds(&dir, &mut index.notes) {
                index.sound_files.insert(format!("sounds/{relative}").to_lowercase());
            }
        }

        // merge: one entry per name, the copies in precedence order, the facts from the best one
        let mut by_name: BTreeMap<&str, Vec<&Copy>> = BTreeMap::new();
        for copy in game.copies.iter().chain(addon_copies.iter()) {
            by_name.entry(copy.name.as_str()).or_default().push(copy);
        }
        index.entries.reserve(by_name.len());
        for (_, mut group) in by_name {
            // stable sort on the enum's own order (the precedence order)
            group.sort_by_key(|copy| copy.source);
            let top = group[0];
            let facts = Facts::of(&top.properties);
            let mut sources: Vec<SoundSource> = Vec::new();
            for copy in &group {
                if !sources.contains(&copy.source) {
                    sources.push(copy.source);
                }
            }
            for key in top.properties.keys() {
                index.known_keys.insert(key.clone());
            }
            // M1: the addon's `_c` is the only holder and the source next to it no longer carries
            // the name (renamed or removed and not recompiled) — a `Stale` warning, not a `Known`.
            let stale = if sources == [SoundSource::AddonGame] {
                stale_source_of(
                    &group,
                    addon,
                    &index.addon_source_files,
                    &index.addon_unparsed_files,
                )
            } else {
                None
            };
            index.entries.push(SoundEventEntry {
                name: top.name.clone(),
                file: top.file.clone(),
                compiled: group.iter().any(|copy| copy.source.is_compiled()),
                sources,
                type_: facts.type_,
                base: facts.base,
                volume: facts.volume,
                vsnds: facts.vsnds,
                children: facts.children,
                position: facts.position,
                properties: Value::Object(top.properties.clone()),
            });
            if let Some(source) = stale {
                index.stale.insert(top.name.clone(), source);
            }
        }
        if !non_events.is_empty() {
            // One note for the whole build: the compiler writes scalar root members next to the
            // events and they are not events (one note per file would drown every real note).
            let mut names: Vec<String> = non_events.iter().cloned().collect();
            names.sort();
            names.truncate(8);
            index.notes.push(format!(
                "{} root member(s) are not event objects and are not indexed ({}); the compiler \
                 writes these next to the events",
                non_events.len(),
                names.join(", ")
            ));
        }
        index.game = game;
        index.built_ms = started.elapsed().as_millis() as u64;
        index
    }

    /// True while the VPK and addon soundevent files are unchanged since the build.
    pub fn fresh(&self) -> bool {
        self.watched
            .iter()
            .all(|(path, mtime)| *mtime == file_mtime(path))
    }

    /// True while both game VPKs still carry the mtime they had at build time — the gate that lets
    /// [`index_for_report`] answer a changed addon with [`SoundEventIndex::rescan_addon`] instead
    /// of a full [`SoundEventIndex::build`] (review M3). Always false on the `Default` stub (it has
    /// no VPK half to reuse).
    pub fn vpk_unchanged(&self) -> bool {
        !self.vpk_watched.is_empty()
            && self
                .vpk_watched
                .iter()
                .all(|(path, mtime)| *mtime == file_mtime(path))
    }

    /// `Some(source file)` when `entry`'s only holder is the addon's compiled `_c` and the
    /// `.vsndevts` source next to that `_c` is present, **decoded**, and no longer carries the
    /// name: renamed or removed in the source, not recompiled ([`SoundNameVerdict::Stale`], review
    /// M1). A source that did not decode proves nothing and answers `None` (review N3).
    pub fn stale_source(&self, entry: &SoundEventEntry) -> Option<&str> {
        self.stale.get(&entry.name).map(String::as_str)
    }

    /// Re-read the addon sources only (VPK entries reused). The returned index carries this
    /// call's own `built_ms`; the two VPK trees are never re-opened or re-decoded, so this works
    /// even when the paks became unreadable since the build. Review N1/N10: the shared `Arc`
    /// `GamePart` is handed over as-is (a pointer copy, no deep clone) and `assemble` copies its
    /// watches/assets/notes into the new index.
    pub fn rescan_addon(&self) -> Self {
        Self::assemble(
            &self.app_root,
            &self.addon,
            Arc::clone(&self.game),
            Instant::now(),
        )
    }

    /// Exact (case-sensitive) lookup — the engine hashes names, so case is significant (A1).
    pub fn get(&self, name: &str) -> Option<&SoundEventEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Case-insensitive exact matches first (sorted), then names sharing at least a 3-character
    /// prefix with the request, ranked by shared-prefix length and then alphabetically — the
    /// `model_catalog` ranking, minus its directory rule (a soundevent name has no directories).
    /// The request itself is never a suggestion — except when the request is whitespace-padded, in
    /// which case its trimmed spelling is the answer (`"mcp.snd.point "` -> `mcp.snd.point`,
    /// review L2).
    pub fn suggestions(&self, name: &str, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let needle = name.trim();
        // Only skip "the request itself" when the request *is* the trimmed name: a padded request
        // wants its trimmed spelling suggested.
        let skip_self = name == needle;
        let lowered = needle.to_lowercase();
        let mut exact: Vec<&str> = Vec::new();
        let mut near: Vec<(usize, &str)> = Vec::new();
        for entry in &self.entries {
            if entry.name == needle && skip_self {
                continue; // the request itself is never a suggestion
            }
            let candidate = entry.name.to_lowercase();
            if candidate == lowered {
                exact.push(&entry.name);
                continue;
            }
            let shared = shared_prefix(&lowered, &candidate);
            if shared >= 3 {
                near.push((shared, &entry.name));
            }
        }
        exact.sort();
        near.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        exact
            .into_iter()
            .chain(near.into_iter().map(|(_, name)| name))
            .take(limit)
            .map(str::to_string)
            .collect()
    }

    /// Base-first merge of `name`'s `base` chain. The chain starts at the topmost base and ends
    /// with `name` itself, so folding it left to right lets the child override the base; a name
    /// that appears twice cuts the walk and reports `cycle:true`. `None` = the name is unknown.
    pub fn resolve_chain(&self, name: &str) -> Option<ResolvedEvent> {
        self.get(name)?;
        let mut up: Vec<String> = vec![name.to_string()];
        let mut cycle = false;
        loop {
            let current = up.last().expect("the chain starts with the name").clone();
            let Some(base) = self.get(&current).and_then(|entry| entry.base.clone()) else {
                break;
            };
            if up.contains(&base) {
                cycle = true;
                break;
            }
            if up.len() >= CHAIN_MAX {
                break;
            }
            up.push(base);
        }
        up.reverse();
        let mut merged = Map::new();
        for name in &up {
            if let Some(Value::Object(properties)) = self.get(name).map(|entry| &entry.properties) {
                for (key, value) in properties {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
        Some(ResolvedEvent {
            chain: up,
            merged,
            cycle,
        })
    }

    /// The union of every indexed event's property keys (WP-S2's unknown-key warning checks
    /// against this: the compiler accepts an invented key silently, so the corpus is the only
    /// authority on what exists).
    pub fn known_property_keys(&self) -> &BTreeSet<String> {
        &self.known_keys
    }

    /// The lowercased `sounds/**` asset paths the index saw (`vsnds[].known`).
    pub fn sound_files(&self) -> &BTreeSet<String> {
        &self.sound_files
    }

    /// `Some(true/false)` when `path` names an asset the index can check (the path as written and
    /// its compiled `.vsnd_c` spelling); `None` for a path that cannot be looked up at all (an
    /// absolute path or one with a `..` segment).
    pub fn sound_asset_known(&self, path: &str) -> Option<bool> {
        let text = path.trim().replace('\\', "/").to_lowercase();
        if text.is_empty()
            || text.starts_with('/')
            || text.split('/').any(|segment| segment == "..")
        {
            return None;
        }
        Some(self.sound_files.contains(&text) || self.sound_files.contains(&format!("{text}_c")))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn entries(&self) -> &[SoundEventEntry] {
        &self.entries
    }
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
    pub fn files(&self) -> usize {
        self.files
    }
    pub fn built_ms(&self) -> u64 {
        self.built_ms
    }

    /// A rough size of the `properties` JSON the whole index holds (the A2 memory question;
    /// the corpus test prints it).
    pub fn properties_json_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| json_len(&entry.properties))
            .sum()
    }
}

/// The JSON size of one value as `serde_json` would render it (counting the quotes and commas
/// the encoder inserts; a rough lower bound is enough for the A2 decision).
fn json_len(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(key, member)| json_len(&Value::String(key.clone())) + 1 + json_len(member) + 1)
                .sum::<usize>()
        }
        Value::Array(items) => {
            2 + items
                .iter()
                .map(|item| json_len(item) + 1)
                .sum::<usize>()
        }
        Value::String(text) => text.len() + 2,
        Value::Null => 4,
        other => other.to_string().len(),
    }
}

/// The two game VPKs: both required, their `soundevents/**/*.vsndevts_c` decoded, their
/// `sounds/**` asset paths collected.
fn read_game(app_root: &Path) -> Result<GamePart, IndexError> {
    let mut part = GamePart::default();
    for (pak, relative, source) in [
        ("csgo", "game/csgo/pak01_dir.vpk", SoundSource::VpkCsgo),
        ("core", "game/core/pak01_dir.vpk", SoundSource::VpkCore),
    ] {
        let path = app_root.join(relative);
        if !path.is_file() {
            return Err(IndexError::VpkMissing {
                path: path.clone(),
                detail: format!("the game's {pak} directory VPK is missing"),
            });
        }
        let vpk = VpkDir::open(&path).map_err(|error| IndexError::VpkUnreadable {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        part.watched.push((path.clone(), file_mtime(&path)));
        let mut sound_files_capped = false;
        for entry in vpk.paths() {
            if !is_sound_asset(entry) {
                continue;
            }
            if part.sound_files.len() >= SOUND_FILES_MAX {
                sound_files_capped = true;
                continue;
            }
            part.sound_files.insert(entry.to_string());
        }
        if sound_files_capped {
            part.notes.push(format!(
                "the pak {pak} carries more than {SOUND_FILES_MAX} sound assets: the \
                 `vsnds[].known` set is capped (a name past the cap answers `false`)"
            ));
        }
        let blocks: Vec<String> = vpk
            .paths()
            .filter(|entry| is_soundevents_block(entry))
            .map(str::to_string)
            .collect();
        for entry in blocks {
            match vpk.read(&entry) {
                Ok(bytes) => decode_compiled(
                    &bytes,
                    source,
                    &entry,
                    &mut part.copies,
                    &mut part.notes,
                    &mut part.files,
                    &mut part.non_events,
                ),
                Err(error) => part
                    .notes
                    .push(format!("pak {pak}: {entry} could not be read: {error}")),
            }
        }
    }
    Ok(part)
}

/// An addon file's path relative to the addon's `soundevents` directory, for one of the two addon
/// trees (`game/…` or `content/…`): `Some("soundevents_addon.vsndevts_c")` for the compiled file
/// of the fixture addon. `None` for anything else (a VPK path, another addon).
fn addon_rel(file: &str, addon: &str, tree: &str) -> Option<String> {
    file.strip_prefix(&format!("{tree}/csgo_addons/{addon}/soundevents/"))
        .map(str::to_string)
}

/// The addon source file that should carry a name whose **only** copy is the addon's compiled
/// `_c` (review M1): for every compiled copy whose paired `.vsndevts` is one of the addon-source
/// files the walk found, the source path is returned — the caller has already established that no
/// addon-source copy carries the name, so that source renamed or removed it and the `_c` was never
/// recompiled.
///
/// Review N3: a source that is present but did **not** decode (`addon_unparsed`) is skipped — its
/// parse failure says nothing about what the `_c` carries, so no name of that `_c` is reported
/// stale from it.
fn stale_source_of(
    copies: &[&Copy],
    addon: &str,
    addon_source_files: &BTreeSet<String>,
    addon_unparsed: &BTreeSet<String>,
) -> Option<String> {
    for copy in copies
        .iter()
        .filter(|copy| copy.source == SoundSource::AddonGame)
    {
        let Some(relative) = addon_rel(&copy.file, addon, "game") else {
            continue;
        };
        let Some(stem) = relative.strip_suffix(".vsndevts_c") else {
            continue;
        };
        let source = format!("{stem}.vsndevts");
        if addon_unparsed.contains(&source) {
            continue;
        }
        if addon_source_files.contains(&source) {
            return Some(format!("content/csgo_addons/{addon}/soundevents/{source}"));
        }
    }
    None
}

/// The `sounds/**` suffixes [`SoundEventIndex::sound_asset_known`] can answer for.
const SOUND_ASSET_SUFFIXES: [&str; 4] = [".vsnd", ".vsnd_c", ".wav", ".mp3"];

/// `soundevents/**/*.vsndevts_c` inside a VPK (its paths are lowercase already).
fn is_soundevents_block(path: &str) -> bool {
    path.starts_with("soundevents/") && path.ends_with(".vsndevts_c")
}

/// A `sounds/**` asset the `vsnds[].known` check can answer for.
fn is_sound_asset(path: &str) -> bool {
    path.starts_with("sounds/")
        && SOUND_ASSET_SUFFIXES
            .iter()
            .any(|suffix| has_extension(path, suffix))
}

/// A file's mtime, or `None` when it cannot be read (and so counts as moved: never fresh).
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

/// `name` ends with `ext` (ASCII case-insensitively).
fn has_extension(name: &str, ext: &str) -> bool {
    name.len() >= ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// The shared-prefix length of two strings (ASCII bytes; names are ASCII in practice).
fn shared_prefix(a: &str, b: &str) -> usize {
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(a, b)| a == b)
        .count()
}

/// The files under `dir` whose name ends with one of `extensions` (case-insensitively),
/// relative to `dir`, sorted. Bounded in depth and count, symlinks never followed, every bound
/// hit and every unreadable directory a note.
fn walk_files(dir: &Path, extensions: &[&str], notes: &mut Vec<String>) -> Vec<String> {
    walk(dir, notes, &|name: &str, kind: WalkKind| match kind {
        WalkKind::Dir => true,
        WalkKind::File => extensions
            .iter()
            .any(|extension| has_extension(name, extension)),
    })
}

/// Every file under a `sounds` tree (no extension filter).
fn walk_sounds(dir: &Path, notes: &mut Vec<String>) -> Vec<String> {
    walk(dir, notes, &|_, _| true)
}

#[derive(Clone, Copy)]
enum WalkKind {
    Dir,
    File,
}

/// A bounded, sorted, symlink-free walk. `keep(name, kind)` decides what is collected.
fn walk(
    dir: &Path,
    notes: &mut Vec<String>,
    keep: &dyn Fn(&str, WalkKind) -> bool,
) -> Vec<String> {
    let mut relative: Vec<String> = Vec::new();
    let mut depth_bound_hit = false;
    let mut count_bound_hit = false;
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
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue; // never followed
            }
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            if kind.is_dir() {
                if !keep(&name, WalkKind::Dir) {
                    continue;
                }
                if depth >= WALK_MAX_DEPTH {
                    depth_bound_hit = true;
                    continue;
                }
                stack.push((format!("{prefix}{name}/"), here.join(&name), depth + 1));
            } else if kind.is_file() && keep(&name, WalkKind::File) {
                if relative.len() >= WALK_MAX_FILES {
                    count_bound_hit = true;
                    break;
                }
                relative.push(format!("{prefix}{name}"));
            }
        }
        if count_bound_hit {
            break;
        }
    }
    if depth_bound_hit {
        notes.push(format!(
            "the soundevent walk stopped at the {WALK_MAX_DEPTH}-directory bound and skipped \
             the deeper directories of {}",
            dir.display()
        ));
    }
    if count_bound_hit {
        notes.push(format!(
            "the soundevent walk stopped at its {WALK_MAX_FILES}-file bound in {}",
            dir.display()
        ));
    }
    relative.sort();
    relative.dedup();
    relative
}

// -------------------------------------------------------------------------------------------
// the state cache
// -------------------------------------------------------------------------------------------

/// The cached index for `state`, plus whether this call built it (`list_soundevents`'s
/// `index.rebuilt`).
///
/// Review M3: what was built last is **never thrown away**. A stale cache (a `fresh()` miss) and a
/// cache the writers' [`HammerState::soundevent_index_invalidate`] just dropped (it is parked on
/// `HammerState::soundevent_index_last`) both become the *previous* index here, and while both game
/// VPKs are untouched ([`SoundEventIndex::vpk_unchanged`]) the new index is
/// [`SoundEventIndex::rescan_addon`] of it: the 193 decoded pak blocks are reused, so an
/// `edit_soundevents`/`compile_soundevents` costs one addon walk instead of a full decode. A moved
/// pak (or a previous index that carries no VPK half) still rebuilds everything.
pub async fn index_for_report(
    state: &HammerState,
) -> Result<(Arc<SoundEventIndex>, bool), ToolError> {
    let wanted = state.addon();
    let previous: Option<Arc<SoundEventIndex>> = {
        let cached = state
            .soundevent_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cached.as_ref() {
            // keyed by addon (WP1.1): an index of another addon is neither served nor rescanned
            // (the `Default` stub, addon "", is the test seam's addon-less index)
            Some(index) if (index.addon().is_empty() || index.addon() == wanted) && index.fresh() => {
                return Ok((Arc::clone(index), false))
            }
            Some(index) => Some(Arc::clone(index)),
            // the writer's `soundevent_index_invalidate` parked the last index here
            None => state
                .soundevent_index_last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map(Arc::clone),
        }
    };
    let app_root = state.content().app_root.clone();
    let addon = wanted;
    let built = tokio::task::spawn_blocking(move || match previous {
        Some(previous) if previous.addon() == addon && previous.vpk_unchanged() => Ok(previous.rescan_addon()),
        _ => SoundEventIndex::build(&app_root, &addon),
    })
    .await
    .map_err(|error| {
        ToolError::internal(format!("{PLAN}: the index build task failed: {error}"))
    })?
    .map_err(index_tool_error)?;
    let index = Arc::new(built);
    *state
        .soundevent_index
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&index));
    // the parked index is only a rescan source: drop it so one server holds one index
    *state
        .soundevent_index_last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    Ok((index, true))
}

/// The cached index for this server (built in `spawn_blocking` on first use, rebuilt when stale
/// or invalidated).
pub async fn index_for(state: &HammerState) -> Result<Arc<SoundEventIndex>, ToolError> {
    Ok(index_for_report(state).await?.0)
}

/// A failed build as a tool error: `VpkMissing` names the file (never an empty index), an
/// unreadable VPK is `CatalogVpkError` (the model catalog's taxonomy).
pub fn index_tool_error(error: IndexError) -> ToolError {
    match error {
        IndexError::VpkMissing { path, detail } => ToolError::new(
            "VpkMissing",
            "VpkMissing",
            format!(
                "the soundevent index cannot be built: {detail} ({})",
                path.display()
            ),
        )
        .with_data(json!({"path": path.display().to_string()}))
        .retry_safe(true),
        IndexError::VpkUnreadable { path, detail } => ToolError::new(
            "CatalogVpkError",
            "CatalogVpkError",
            format!(
                "the soundevent index cannot read the game VPK {}: {detail}",
                path.display()
            ),
        )
        .with_data(json!({"path": path.display().to_string()}))
        .retry_safe(true),
    }
}

/// The one gate every `sound`-typed keyvalue goes through. `FilePath` never reaches the index
/// (`ambient_generic.message` accepts a direct `.wav`/`.vsnd` path), `Empty` is the "clear it"
/// value; a name whose only compiled copy is an addon `_c` its source no longer carries is
/// [`SoundNameVerdict::Stale`]; an unknown name is `Unknown` **with suggestions** (the caller may
/// refuse or warn).
pub async fn check_sound_name(
    state: &HammerState,
    name: &str,
) -> Result<SoundNameVerdict, ToolError> {
    if name.trim().is_empty() {
        return Ok(SoundNameVerdict::Empty);
    }
    if is_file_path(name) {
        return Ok(SoundNameVerdict::FilePath);
    }
    let index = index_for(state).await?;
    if let Some(entry) = index.get(name) {
        if !entry.compiled {
            return Ok(SoundNameVerdict::SourceOnly {
                file: entry.file.clone(),
            });
        }
        if let Some(file) = index.stale_source(entry) {
            return Ok(SoundNameVerdict::Stale {
                file: file.to_string(),
            });
        }
        return Ok(SoundNameVerdict::Known {
            name: entry.name.clone(),
            source: entry.source(),
            file: entry.file.clone(),
        });
    }
    Ok(SoundNameVerdict::Unknown {
        suggestions: index.suggestions(name, 5),
    })
}

/// Is this keyvalue a *path* rather than an event name? `/` (or `\`) or a `.vsnd`/`.vsnd_c`/
/// `.wav`/`.mp3` suffix — `ambient_generic.message`'s direct-file form, never looked up.
pub fn is_file_path(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.contains('/') || trimmed.contains('\\') {
        return true;
    }
    SOUND_ASSET_SUFFIXES
        .iter()
        .any(|suffix| has_extension(trimmed, suffix))
}
