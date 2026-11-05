//! Entity class database from the game's FGD files (`<game>/csgo/csgo.fgd` and its `@include`s,
//! resolved in `csgo/` then `core/`). Classes carry their resolved keyvalues (base classes first,
//! child overrides), inputs and outputs. `validate` checks keyvalues before a live edit.
//! Plan: target/entities-plan.md §4.1 WP-F.
use crate::state::HammerState;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

mod lexer;
mod parser;

use parser::{ParsedItem, RawClass};

#[derive(Debug, thiserror::Error)]
pub enum FgdError {
    #[error("FGD database not implemented yet")]
    NotImplemented,
    #[error("FGD unavailable: {path} not found")]
    Unavailable { path: PathBuf },
    #[error("{file}:{line}: {reason}")]
    Parse {
        file: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("@include cycle: {chain}")]
    IncludeCycle { chain: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Point,
    Solid,
    Base,
    Other,
}

/// The prop tags a `tags( … )` header helper can carry (`base.fgd`:
/// `tags( PropStatic )` on `prop_static`, `tags( PropDynamic )` on `prop_dynamic_base`,
/// `tags( PropPhysics )` on `BasePropPhysics`). Case-insensitive; a `tags()` with several
/// arguments (`tags( Info, PlayerPosition )`) is one helper whose args carry them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropTag {
    Static,
    Dynamic,
    Physics,
}

impl PropTag {
    /// The tag's FGD spelling (`"PropStatic"` et al.).
    pub fn as_str(self) -> &'static str {
        match self {
            PropTag::Static => "PropStatic",
            PropTag::Dynamic => "PropDynamic",
            PropTag::Physics => "PropPhysics",
        }
    }
}

/// The recognized prop tag in one `tags( … )` helper's raw args (`"PropStatic"`,
/// `"Info, PlayerPosition"`, ...), or `None` when the args carry none.
pub fn prop_tag_from_args(args: &str) -> Option<PropTag> {
    args.split(',').filter_map(prop_tag_from_arg).next()
}

/// The recognized prop tag in one tag argument (`"PropStatic"`, `"PropPhysics"`).
fn prop_tag_from_arg(arg: &str) -> Option<PropTag> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "propstatic" => Some(PropTag::Static),
        "propdynamic" => Some(PropTag::Dynamic),
        "propphysics" => Some(PropTag::Physics),
        _ => None,
    }
}

/// True when a `tags( … )` helper's raw args (`"Trigger"`, `"Info, PlayerPosition"`) list the
/// trigger tag (`base.fgd`: `tags( Trigger )` on `TriggerOnce`). Case-insensitive, like the prop
/// tags; a multi-argument `tags()` is one helper whose args carry them all.
pub fn trigger_tag_from_args(args: &str) -> bool {
    args.split(',')
        .any(|arg| arg.trim().eq_ignore_ascii_case("Trigger"))
}

/// True when one `tags( … )` helper's raw args (`"Sound"`, `"Info, PlayerPosition"`) list `tag`:
/// the generic form of [`prop_tag_from_args`]/[`trigger_tag_from_args`] the class-wide
/// [`Database::has_tag`] walk and `list_entity_classes`' `tag` filter use. Case-insensitive, each
/// comma-separated argument trimmed; a multi-argument `tags()` is one helper whose args carry them
/// all.
pub fn tag_from_args(args: &str, tag: &str) -> bool {
    args.split(',')
        .any(|arg| arg.trim().eq_ignore_ascii_case(tag))
}

/// True for the two FGD base class names that mark a trigger (`base.fgd`: `TriggerOnce`, which
/// carries the flags, and `Trigger`, which adds the `On*Touch` outputs).
fn is_trigger_base_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("TriggerOnce") || name.eq_ignore_ascii_case("Trigger")
}

/// One declared spawnflag's state for a decoded `spawnflags` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagState {
    pub bit: u64,
    pub label: String,
    /// True when `bit` is set in the decoded value.
    pub on: bool,
}

/// A decoded `spawnflags` value: the number plus the state of every flag the class declares.
/// `flags` keeps the FGD declaration order (the base class's flags first when a subclass adds
/// its own bits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnflagsView {
    pub value: u64,
    pub flags: Vec<FlagState>,
    /// The bits set in `value` that no declared flag covers (a bitmask; 0 = every bit is known).
    pub unknown_bits: u64,
}

/// Why a `spawnflags` text could not be decoded. A class that declares no `spawnflags` is an
/// error, never a silent 0.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnflagsError {
    #[error("class '{class}' declares no spawnflags keyvalue")]
    NoSpawnflags { class: String },
    #[error("class '{class}': spawnflags text '{text}' is not a non-negative integer")]
    NotInteger { class: String, text: String },
}

/// A spawnflag name/number [`Database::encode_spawnflags`] was given that the class does not
/// declare: `name` is the offending spelling as written, `valid` the declared labels (in FGD
/// order) that may be passed instead.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("class '{class}' has no spawnflag '{name}' (declared: {valid:?})")]
pub struct UnknownSpawnflag {
    pub class: String,
    pub name: String,
    pub valid: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub bit: u64,
    pub label: String,
    pub default_on: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyDef {
    pub name: String,
    /// The FGD type token, lowercased (`string`, `integer`, `float`, `boolean`, `choices`,
    /// `flags`, `target_destination`, `studio`, `color255`, `vector`, ...).
    pub type_: String,
    pub display_name: Option<String>,
    pub default: Option<String>,
    pub description: Option<String>,
    pub choices: Vec<Choice>,
    pub flags: Vec<Flag>,
    /// The class that declared this key (itself or a base).
    pub declared_in: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IoDef {
    pub name: String,
    pub type_: String,
    pub description: Option<String>,
    pub declared_in: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: String,
    pub kind: ClassKind,
    pub description: Option<String>,
    /// Direct bases as written; `resolved_bases` is the full chain (depth-first, base first).
    pub bases: Vec<String>,
    pub resolved_bases: Vec<String>,
    /// Resolved: base keys first, a child's same-named key overrides in place.
    pub keys: Vec<KeyDef>,
    pub inputs: Vec<IoDef>,
    pub outputs: Vec<IoDef>,
    /// Header helpers (`editormodel`, `iconsprite`, `size`, `tags`, ...) as name -> raw args.
    pub helpers: Vec<(String, String)>,
    /// `file:line` of the declaration.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IssueKind {
    UnknownKey,
    BadType,
    BadChoice,
    Forbidden,
}

/// How seriously a `validate` problem is taken: `Error` refuses under `strict`, `Warning` only
/// reports (an integral float on an integer key is accepted by Hammer, so refusing it would be a
/// false positive; brief item 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub key: String,
    pub kind: IssueKind,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Validation {
    pub unknown_class: bool,
    /// Up to 5 class names within edit distance 3 when `unknown_class`.
    pub nearest: Vec<String>,
    pub issues: Vec<Issue>,
}

impl Validation {
    /// True when any issue is an error (`strict` refuses on these; warnings pass).
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|issue| issue.severity == Severity::Error)
    }
}

#[derive(Debug, Default)]
pub struct Database {
    /// Files read, in load order.
    pub files: Vec<PathBuf>,
    /// Non-fatal problems (missing include, skipped block) as `file:line: text`.
    pub warnings: Vec<String>,
    classes: Vec<Class>,
    /// `@exclude`d class names (lowercased) that no later definition revived: hidden from
    /// `class`/`classes`. An `@exclude` followed by a redefinition is visible again.
    excluded: HashSet<String>,
}

/// Key types whose values must be integer text (`node_dest`/`node_id` name node ids).
const INTEGER_TYPES: [&str; 5] = ["integer", "int", "flags", "node_dest", "node_id"];

impl Database {
    /// Parse `<root>/csgo/csgo.fgd` and its includes (uncached).
    pub fn load(root: &Path) -> Result<Database, FgdError> {
        let entry = root.join("csgo").join("csgo.fgd");
        if !entry.is_file() {
            return Err(FgdError::Unavailable { path: entry });
        }
        let mut asm = Assembly::default();
        let mut stack: Vec<PathBuf> = Vec::new();
        let mut done: HashSet<PathBuf> = HashSet::new();
        load_file(&entry, root, &mut stack, &mut done, &mut asm)?;
        Ok(finish(asm))
    }

    pub fn class(&self, name: &str) -> Option<&Class> {
        if self.excluded.contains(&name.to_ascii_lowercase()) {
            return None;
        }
        self.classes
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The first prop tag of `class`: the `tags( … )` helper of the class itself, then of its
    /// resolved base chain (depth-first, the declared order — a first mention wins). Through the
    /// chain a child inherits the tag: `prop_dynamic` has no `tags()` of its own but resolves to
    /// `prop_dynamic_base`'s `tags( PropDynamic )`. Classes the database does not carry (or that
    /// carry no recognizable prop tag) are `None`.
    pub fn prop_tag(&self, class: &str) -> Option<PropTag> {
        let mut names: Vec<&str> = vec![class];
        let mut seen: HashSet<String> =
            names.iter().map(|name| name.to_ascii_lowercase()).collect();
        let mut idx = 0;
        while idx < names.len() {
            let name = names[idx];
            idx += 1;
            let Some(def) = self.class(name) else {
                continue;
            };
            for (helper, args) in &def.helpers {
                if helper.eq_ignore_ascii_case("tags") {
                    if let Some(tag) = prop_tag_from_args(args) {
                        return Some(tag);
                    }
                }
            }
            for base in &def.resolved_bases {
                let lower = base.to_ascii_lowercase();
                if seen.insert(lower) {
                    names.push(base.as_str());
                }
            }
        }
        None
    }

    /// True when `class` is a trigger class: it is a **`@SolidClass`** (a brush entity — only a
    /// Solid class is placed as a `CMapEntity` with mesh children) whose resolved base chain (the
    /// same depth-first walk as [`Database::prop_tag`]) carries a class named `TriggerOnce`/
    /// `Trigger` (`base.fgd`'s `tags( Trigger )` pair) or a `tags( … )` helper whose args list
    /// `Trigger`. Case-insensitive.
    ///
    /// `trigger_once` (base `TriggerOnce`) and `trigger_multiple`/`trigger_push` (base `Trigger`)
    /// are triggers; `func_buyzone` is a `@SolidClass` but not one (base `Targetname` only), and a
    /// class the database does not carry is never a trigger. **Not** triggers, even though their
    /// chain carries the marker: the `@BaseClass` `TriggerOnce`/`Trigger` themselves and any
    /// `@PointClass` with `tags( Trigger )` — neither can be a brush volume, so the kind gate is
    /// part of the predicate (callers must not have to repeat it). Consumers: the trigger tools'
    /// `kind:"trigger"` filter and the material fallback in `set_trigger_volume`.
    pub fn is_trigger_class(&self, class: &str) -> bool {
        if !self
            .class(class)
            .is_some_and(|def| def.kind == ClassKind::Solid)
        {
            return false;
        }
        let mut names: Vec<&str> = vec![class];
        let mut seen: HashSet<String> =
            names.iter().map(|name| name.to_ascii_lowercase()).collect();
        let mut idx = 0;
        while idx < names.len() {
            let name = names[idx];
            idx += 1;
            let Some(def) = self.class(name) else {
                continue;
            };
            if is_trigger_base_name(&def.name) {
                return true;
            }
            for (helper, args) in &def.helpers {
                if helper.eq_ignore_ascii_case("tags") && trigger_tag_from_args(args) {
                    return true;
                }
            }
            for base in &def.resolved_bases {
                let lower = base.to_ascii_lowercase();
                if seen.insert(lower) {
                    names.push(base.as_str());
                }
            }
        }
        false
    }

    /// True when `class` — or any class in its resolved base chain (the same depth-first walk as
    /// [`Database::prop_tag`]) — carries a `tags( … )` header helper whose args list `tag`
    /// (case-insensitive, see [`tag_from_args`]). The generic lookup behind
    /// [`Database::is_sound_class`] and `list_entity_classes`' `tag` filter, so a child inherits
    /// its base's tags (`snd_event_point` has no `tags()` of its own but resolves to
    /// `point_soundevent`'s `tags( Sound )`). A class the database does not carry is never tagged.
    pub fn has_tag(&self, class: &str, tag: &str) -> bool {
        let mut names: Vec<&str> = vec![class];
        let mut seen: HashSet<String> =
            names.iter().map(|name| name.to_ascii_lowercase()).collect();
        let mut idx = 0;
        while idx < names.len() {
            let name = names[idx];
            idx += 1;
            let Some(def) = self.class(name) else {
                continue;
            };
            for (helper, args) in &def.helpers {
                if helper.eq_ignore_ascii_case("tags") && tag_from_args(args, tag) {
                    return true;
                }
            }
            for base in &def.resolved_bases {
                let lower = base.to_ascii_lowercase();
                if seen.insert(lower) {
                    names.push(base.as_str());
                }
            }
        }
        false
    }

    /// True when `class` (or its resolved base chain) carries `tags( Sound )` — the FGD marker of
    /// every class that plays a soundevent: `ambient_generic`, `point_soundevent` and its
    /// `snd_event_*` subclasses, `env_soundscape` and its `snd_soundscape*` subclasses.
    ///
    /// The tag is the marker, not the key type: `trigger_soundscape` carries a `soundscape`
    /// keyvalue but **no** `tags( Sound )` in Valve's files (its `soundscape` key is a legacy
    /// `.vsc` name, not a soundevent), so it is not a sound class. A class the database does not
    /// carry is never one either.
    pub fn is_sound_class(&self, class: &str) -> bool {
        self.has_tag(class, "Sound")
    }

    /// The resolved keyvalues of `class` whose FGD type is `sound` — the keys whose text names a
    /// soundevent, in declaration order (`point_soundevent.soundName`, inherited by every
    /// `snd_event_*`; `env_soundscape.soundevent`, inherited by `snd_soundscape`;
    /// `ambient_generic.message`, the legacy file-or-event form). `env_soundscape.soundscape` is
    /// **not** one (its type is `string`: a legacy `.vsc` name). Empty for a class the database
    /// does not carry and for one that declares none.
    pub fn sound_keys(&self, class: &str) -> Vec<&KeyDef> {
        self.class(class)
            .map(|def| {
                def.keys
                    .iter()
                    .filter(|key| key.type_ == "sound")
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The resolved `spawnflags` keyvalue of `class` (itself or through the base chain) — **only
    /// when it is declared with the `flags` type**. `None` when the class — or its whole chain —
    /// declares none, and also when it declares `spawnflags` with another type
    /// (`spawnflags(integer)` exists in Valve's files): such a key is a plain integer, so there is
    /// no flag list to decode — returning it would make `decode_spawnflags` report every bit as
    /// unknown and `encode_spawnflags` reject every label with an empty `valid` list.
    ///
    /// The flags of a class that redeclares `spawnflags(flags)` are merged with its base's (see
    /// `upsert_key`), so `trigger_push` carries `TriggerOnce`'s 12 bits plus its own 128/256.
    pub fn spawnflags_def(&self, class: &str) -> Option<&KeyDef> {
        self.class(class)?
            .keys
            .iter()
            .find(|k| k.name.eq_ignore_ascii_case("spawnflags") && k.type_ == "flags")
    }

    /// Decode the stored `spawnflags` text (`"4097"`, empty = 0) into the value plus every declared
    /// flag's state. Bits the class does not declare are reported as `unknown_bits` instead of
    /// being dropped. The text must be a non-negative integer after trimming.
    pub fn decode_spawnflags(
        &self,
        class: &str,
        text: &str,
    ) -> Result<SpawnflagsView, SpawnflagsError> {
        let Some(def) = self.spawnflags_def(class) else {
            return Err(SpawnflagsError::NoSpawnflags {
                class: class.to_string(),
            });
        };
        let trimmed = text.trim();
        let value = if trimmed.is_empty() {
            0
        } else {
            trimmed
                .parse::<u64>()
                .map_err(|_| SpawnflagsError::NotInteger {
                    class: class.to_string(),
                    text: text.to_string(),
                })?
        };
        let mut known = 0u64;
        let flags = def
            .flags
            .iter()
            .map(|flag| {
                known |= flag.bit;
                FlagState {
                    bit: flag.bit,
                    label: flag.label.clone(),
                    on: value & flag.bit != 0,
                }
            })
            .collect();
        Ok(SpawnflagsView {
            value,
            flags,
            unknown_bits: value & !known,
        })
    }

    /// The `spawnflags` value that turns on every flag the class declares `default_on` (the OR of
    /// those bits): `trigger_once` -> `4097` (`1` Clients | `4096` mass accounting). A class
    /// without `spawnflags` has no defaults: 0.
    pub fn default_spawnflags(&self, class: &str) -> u64 {
        self.spawnflags_def(class)
            .map(|def| {
                def.flags
                    .iter()
                    .filter(|flag| flag.default_on)
                    .fold(0u64, |acc, flag| acc | flag.bit)
            })
            .unwrap_or(0)
    }

    /// Encode flag spellings into a `spawnflags` value: each value is a declared label
    /// (case-insensitive, exact) or an integer that must equal a declared bit. The first value
    /// that names nothing returns [`UnknownSpawnflag`] with the declared labels; an empty slice is
    /// 0. A class without `spawnflags` declares nothing, so every non-empty value is unknown.
    pub fn encode_spawnflags(
        &self,
        class: &str,
        values: &[Value],
    ) -> Result<u64, UnknownSpawnflag> {
        let def = self.spawnflags_def(class);
        let valid = |def: Option<&KeyDef>| -> Vec<String> {
            def.map(|def| {
                def.flags
                    .iter()
                    .filter(|flag| !flag.label.is_empty())
                    .map(|flag| flag.label.clone())
                    .collect()
            })
            .unwrap_or_default()
        };
        let mut out = 0u64;
        for value in values {
            match def.and_then(|def| spawnflag_bit(def, value)) {
                Some(bit) => out |= bit,
                None => {
                    return Err(UnknownSpawnflag {
                        class: class.to_string(),
                        name: spawnflag_name(value),
                        valid: valid(def),
                    })
                }
            }
        }
        Ok(out)
    }

    /// The `auto_apply_material` value of `class`'s `metadata { … }` header helper (the class
    /// first, then its resolved base chain — the same walk as [`Database::prop_tag`]; `base.fgd`'s
    /// `TriggerOnce` carries `auto_apply_material = "materials/tools/toolstrigger.vmat"`). `None`
    /// when no class in the chain carries the helper or the key: the caller must not invent one.
    pub fn auto_apply_material(&self, class: &str) -> Option<String> {
        let mut names: Vec<&str> = vec![class];
        let mut seen: HashSet<String> =
            names.iter().map(|name| name.to_ascii_lowercase()).collect();
        let mut idx = 0;
        while idx < names.len() {
            let name = names[idx];
            idx += 1;
            let Some(def) = self.class(name) else {
                continue;
            };
            for (helper, args) in &def.helpers {
                if helper.eq_ignore_ascii_case("metadata") {
                    if let Some(value) = metadata_value(args, "auto_apply_material") {
                        if !value.is_empty() {
                            return Some(value);
                        }
                    }
                }
            }
            for base in &def.resolved_bases {
                let lower = base.to_ascii_lowercase();
                if seen.insert(lower) {
                    names.push(base.as_str());
                }
            }
        }
        None
    }

    /// True when `class` declares a `model` keyvalue of the FGD `studio` type (itself or through
    /// `base()` resolution) — the marker the model-entity classes carry (Studiomodel's
    /// `model(studio)`; `base.fgd`). Used by the prop tool's `includeOtherModelEntities`.
    pub fn declares_model_studio(&self, class: &str) -> bool {
        self.class(class).is_some_and(|def| {
            def.keys
                .iter()
                .any(|k| k.name.eq_ignore_ascii_case("model") && k.type_ == "studio")
        })
    }

    /// Classes whose name contains `filter` (case-insensitive), of the given kinds, sorted by name.
    /// An empty `kinds` slice means all kinds.
    pub fn classes(&self, filter: Option<&str>, kinds: &[ClassKind], limit: usize) -> Vec<&Class> {
        let needle = filter.map(|f| f.to_ascii_lowercase());
        let mut v: Vec<&Class> = self
            .classes
            .iter()
            .filter(|c| !self.excluded.contains(&c.name.to_ascii_lowercase()))
            .filter(|c| kinds.is_empty() || kinds.contains(&c.kind))
            .filter(|c| {
                needle
                    .as_deref()
                    .map(|n| c.name.to_ascii_lowercase().contains(n))
                    .unwrap_or(true)
            })
            .collect();
        v.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        v.truncate(limit);
        v
    }

    /// Check `keyvalues` (values as JSON; strings expected) against `class`.
    pub fn validate(&self, class: &str, keyvalues: &Map<String, Value>) -> Validation {
        let Some(def) = self.class(class) else {
            return Validation {
                unknown_class: true,
                nearest: self.nearest(class),
                issues: Vec::new(),
            };
        };
        let mut issues = Vec::new();
        for (key, value) in keyvalues {
            if key.eq_ignore_ascii_case("classname") {
                issues.push(Issue {
                    key: key.clone(),
                    kind: IssueKind::Forbidden,
                    severity: Severity::Error,
                    message: "key 'classname' cannot be set through keyvalues (the class is the class parameter)".to_string(),
                });
                continue;
            }
            // The engine-managed keys are always accepted. They are not declared by every FGD
            // class, but a read-side pass over a live entity's keyvalues must not flag them; the
            // mutators refuse `origin`/`angles`/`scales` themselves before validation
            // (`entities_live::keyvalues_arg`), so this set can never smuggle them in. The
            // editor-added `local.*` family is deliberately **not** whitelisted: it is declared by
            // the Parentname/WeaponCore base classes in the real FGDs, so a class that carries it
            // resolves it normally (brief item 12 / opus #14, review 1 #10).
            let lower = key.to_ascii_lowercase();
            if matches!(lower.as_str(), "origin" | "angles" | "targetname") {
                continue;
            }
            let Some(kd) = def
                .keys
                .iter()
                .find(|k| k.name.to_ascii_lowercase() == lower)
            else {
                issues.push(Issue {
                    key: key.clone(),
                    kind: IssueKind::UnknownKey,
                    severity: Severity::Error,
                    message: format!("key '{key}' is not declared by class '{}'", def.name),
                });
                continue;
            };
            // Render the JSON value to the text the FGD stores.
            let text = match value {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(true) => Some("1".to_string()),
                Value::Bool(false) => Some("0".to_string()),
                Value::Null => None,
                Value::Array(_) | Value::Object(_) => {
                    issues.push(Issue {
                        key: key.clone(),
                        kind: IssueKind::BadType,
                        severity: Severity::Error,
                        message: format!("key '{key}' expects {} (type '{}')", expected_text(kd), kd.type_),
                    });
                    continue;
                }
            };
            let Some(text) = text else { continue };
            let type_ = kd.type_.as_str();
            if INTEGER_TYPES.contains(&type_) && text.trim().parse::<i64>().is_err() {
                // `serde_json` renders an integral f64 as "4.0"; Hammer accepts that value, so it
                // is a warning (normalized for the check), not a type error (brief item 12).
                let integral = text
                    .trim()
                    .parse::<f64>()
                    .is_ok_and(|number| number.is_finite() && number.fract() == 0.0);
                issues.push(if integral {
                    Issue {
                        key: key.clone(),
                        kind: IssueKind::BadType,
                        severity: Severity::Warning,
                        message: format!(
                            "key '{key}' expects an integer value (type '{}'); '{text}' is an \
                             integral float, which Hammer normalizes (accepted)",
                            kd.type_
                        ),
                    }
                } else {
                    Issue {
                        key: key.clone(),
                        kind: IssueKind::BadType,
                        severity: Severity::Error,
                        message: format!(
                            "key '{key}' expects an integer value (type '{}'), got '{text}'",
                            kd.type_
                        ),
                    }
                });
                continue;
            }
            if type_ == "float" && text.trim().parse::<f64>().is_err() {
                issues.push(Issue {
                    key: key.clone(),
                    kind: IssueKind::BadType,
                    severity: Severity::Error,
                    message: format!(
                        "key '{key}' expects a float value (type '{}'), got '{text}'",
                        kd.type_
                    ),
                });
                continue;
            }
            if type_ == "boolean" && !matches!(text.as_str(), "0" | "1") {
                issues.push(Issue {
                    key: key.clone(),
                    kind: IssueKind::BadType,
                    severity: Severity::Error,
                    message: format!(
                        "key '{key}' expects \"0\" or \"1\" (type '{}'), got '{text}'",
                        kd.type_
                    ),
                });
                continue;
            }
            if !kd.choices.is_empty() && kd.choices.iter().all(|c| c.value != text) {
                let listed: Vec<&str> = kd.choices.iter().map(|c| c.value.as_str()).collect();
                issues.push(Issue {
                    key: key.clone(),
                    kind: IssueKind::BadChoice,
                    severity: Severity::Error,
                    message: format!(
                        "key '{key}' expects one of the listed choice values [{}], got '{text}'",
                        listed.join(", ")
                    ),
                });
            }
        }
        Validation {
            unknown_class: false,
            nearest: Vec::new(),
            issues,
        }
    }

    /// Up to 5 known class names within case-insensitive edit distance 3, sorted by distance
    /// then name.
    fn nearest(&self, class: &str) -> Vec<String> {
        let target = class.to_ascii_lowercase();
        let mut cands: Vec<(usize, &String)> = self
            .classes
            .iter()
            .map(|c| {
                let name = &c.name;
                (levenshtein(&target, &name.to_ascii_lowercase()), name)
            })
            .filter(|(d, _)| *d <= 3)
            .collect();
        cands.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.to_ascii_lowercase().cmp(&b.1.to_ascii_lowercase()))
                .then_with(|| a.1.cmp(b.1))
        });
        cands.into_iter().take(5).map(|(_, n)| n.clone()).collect()
    }
}

fn expected_text(kd: &KeyDef) -> &'static str {
    if INTEGER_TYPES.contains(&kd.type_.as_str()) {
        "an integer value"
    } else if kd.type_ == "float" {
        "a float value"
    } else if kd.type_ == "boolean" {
        "\"0\" or \"1\""
    } else {
        "a text value"
    }
}

/// The bit `value` names in a `spawnflags` key: a declared label (case-insensitive, exact; an
/// empty label is not a spelling) or an integer that equals a declared bit. `None` names nothing.
fn spawnflag_bit(def: &KeyDef, value: &Value) -> Option<u64> {
    let text = match value {
        Value::String(text) => text.trim().to_string(),
        other => spawnflag_name(other),
    };
    if let Some(flag) = def
        .flags
        .iter()
        .find(|flag| !flag.label.is_empty() && flag.label.eq_ignore_ascii_case(&text))
    {
        return Some(flag.bit);
    }
    let bit: u64 = text.parse().ok()?;
    def.flags
        .iter()
        .find(|flag| flag.bit == bit)
        .map(|flag| flag.bit)
}

/// The spelling of a `spawnflags` value as an error reports it: a string as written, a number as
/// its JSON text, anything else (bool/null/array/object) as its compact JSON.
fn spawnflag_name(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        other => other.to_string(),
    }
}

/// The value of `key` in a `metadata { … }` helper body: `ident = value` pairs whose value is a
/// quoted string (the quotes stripped, escapes kept as written) or a bare word. Quoted strings
/// elsewhere in the body are skipped, so a key name inside another value cannot match.
fn metadata_value(args: &str, key: &str) -> Option<String> {
    let chars: Vec<char> = args.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '"' {
            i = skip_quoted(&chars, i)?;
            continue;
        }
        if !is_meta_word_char(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_meta_word_char(chars[i]) {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars.get(j) != Some(&'=') {
            continue; // a bare word (`true`): not an assignment
        }
        j += 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let value = match chars.get(j)? {
            '"' => {
                let end = skip_quoted(&chars, j)?;
                chars[j + 1..end - 1].iter().collect()
            }
            _ => {
                let value_start = j;
                while j < chars.len() && is_meta_word_char(chars[j]) {
                    j += 1;
                }
                chars[value_start..j].iter().collect()
            }
        };
        if ident.eq_ignore_ascii_case(key) {
            return Some(value);
        }
        i = j;
    }
    None
}

/// The index after the quoted string that opens at `start` (its closing quote); `None` when the
/// string is unterminated. Always advances past `start`.
fn skip_quoted(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '"' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// The metadata body's word characters (the lexer's `[A-Za-z0-9_.-]`).
fn is_meta_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Case-insensitive Levenshtein distance over `char`s.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ---------------------------------------------------------------- assembly

/// One declaration in file order: an `@exclude Name` or a class definition (an index into
/// `Assembly::raws`). The order decides which definitions survive: `@exclude` hides the
/// definitions seen so far, a later definition of the same name brings the class back (Valve's
/// "exclude and redefine" idiom, e.g. `@exclude env_sky` + a new `env_sky` in csgo.fgd).
enum Decl {
    Exclude(String),
    Class(usize),
}

/// Everything parsed before classes are resolved.
#[derive(Default)]
struct Assembly {
    files: Vec<PathBuf>,
    warnings: Vec<String>,
    /// Class definitions and `@exclude`s in file order.
    decls: Vec<Decl>,
    /// Class declarations in file order with the defining file.
    raws: Vec<(RawClass, String)>,
}

/// Parse `path` and its `@include` closure into `asm`.
fn load_file(
    path: &Path,
    root: &Path,
    stack: &mut Vec<PathBuf>,
    done: &mut HashSet<PathBuf>,
    asm: &mut Assembly,
) -> Result<(), FgdError> {
    let canon = std::fs::canonicalize(path).map_err(|e| FgdError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if stack.contains(&canon) {
        let mut chain: Vec<String> = stack.iter().map(|p| p.display().to_string()).collect();
        chain.push(canon.display().to_string());
        return Err(FgdError::IncludeCycle {
            chain: chain.join(" -> "),
        });
    }
    if done.contains(&canon) {
        return Ok(()); // already processed (diamond include)
    }
    stack.push(canon.clone());
    done.insert(canon);
    asm.files.push(path.to_path_buf());
    let text = std::fs::read_to_string(path).map_err(|e| FgdError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let parsed =
        parser::parse_file(path, &text).map_err(|(line, reason)| FgdError::Parse {
            file: path.to_path_buf(),
            line,
            reason,
        })?;
    for (name, line) in &parsed.includes {
        match resolve_include(name, path, root) {
            Some(inc) => load_file(&inc, root, stack, done, asm)?,
            None => asm.warnings.push(format!(
                "{}:{}: @include \"{name}\" not found (searched the including file's directory, csgo/, core/)",
                path.display(),
                line
            )),
        }
    }
    let file_disp = path.display().to_string();
    for (line, text) in parsed.warnings {
        asm.warnings.push(format!("{file_disp}:{line}: {text}"));
    }
    for item in parsed.items {
        match item {
            ParsedItem::Exclude(name) => asm.decls.push(Decl::Exclude(name)),
            ParsedItem::Class(raw) => {
                asm.decls.push(Decl::Class(asm.raws.len()));
                asm.raws.push((raw, file_disp.clone()));
            }
        }
    }
    stack.pop();
    Ok(())
}

/// `@include "x"` resolves in the including file's directory, then `<root>/csgo/`, then
/// `<root>/core/`.
fn resolve_include(name: &str, from: &Path, root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = from.parent() {
        candidates.push(dir.join(name));
    }
    candidates.push(root.join("csgo").join(name));
    candidates.push(root.join("core").join(name));
    candidates.into_iter().find(|p| p.is_file())
}

/// Duplicate/`@exclude`/override handling and base resolution: builds the final `Database`.
fn finish(asm: Assembly) -> Database {
    let mut warnings = asm.warnings;
    let raws = asm.raws;

    // Walk the declarations in file order. `slots[i]` holds raws[i] while that definition is
    // live; `@exclude` drops the live definition of the name (a later one revives it, without a
    // duplicate warning — the earlier definition is gone), a plain redefinition is a duplicate:
    // the later one wins and is warned about.
    let mut slots: Vec<Option<(RawClass, String)>> = raws.iter().map(|_| None).collect();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut excluded: HashSet<String> = HashSet::new();
    let mut overrides: Vec<(RawClass, String)> = Vec::new();
    for decl in asm.decls {
        match decl {
            Decl::Exclude(name) => {
                excluded.insert(name.clone());
                if let Some(idx) = by_name.remove(&name) {
                    slots[idx] = None;
                }
            }
            Decl::Class(idx) => {
                let (raw, file) = &raws[idx];
                if raw.is_override {
                    overrides.push((raw.clone(), file.clone()));
                    continue;
                }
                let key = raw.name.to_ascii_lowercase();
                excluded.remove(&key); // a definition revives an excluded name
                if let Some(&prev) = by_name.get(&key) {
                    warnings.push(format!(
                        "{}:{}: duplicate class '{}' (redefines the earlier definition)",
                        file, raw.line, raw.name
                    ));
                    slots[prev] = None; // later definition wins
                }
                by_name.insert(key, idx);
                slots[idx] = Some((raw.clone(), file.clone()));
            }
        }
    }

    // The surviving definitions, in file order, and their new indices.
    let mut classes: Vec<(RawClass, String)> = Vec::new();
    let mut moved: HashMap<usize, usize> = HashMap::new();
    for (idx, slot) in slots.into_iter().enumerate() {
        if let Some(def) = slot {
            moved.insert(idx, classes.len());
            classes.push(def);
        }
    }
    let mut by_name: HashMap<String, usize> = by_name
        .into_iter()
        .map(|(name, idx)| (name, moved[&idx]))
        .collect();

    for (ov, file) in overrides {
        let key = ov.name.to_ascii_lowercase();
        if let Some(&idx) = by_name.get(&key) {
            merge_override(&mut classes[idx].0, ov);
        } else {
            warnings.push(format!(
                "{}:{}: @OverrideClass '{}' has no matching class; kept as a standalone class",
                file, ov.line, ov.name
            ));
            by_name.insert(key, classes.len());
            classes.push((ov, file));
        }
    }

    // Base resolution, memoized.
    let mut resolved: HashMap<usize, Resolved> = HashMap::new();
    for idx in 0..classes.len() {
        resolve(idx, &classes, &by_name, &mut resolved, &mut warnings);
    }

    let out_classes = classes
        .iter()
        .enumerate()
        .map(|(idx, (raw, file))| {
            let (chain, keys, inputs, outputs) = &resolved[&idx];
            Class {
                name: raw.name.clone(),
                kind: raw.kind,
                description: raw.description.clone(),
                bases: raw.bases.clone(),
                resolved_bases: chain.iter().map(|&b| classes[b].0.name.clone()).collect(),
                keys: keys.clone(),
                inputs: inputs.clone(),
                outputs: outputs.clone(),
                helpers: raw.helpers.clone(),
                source: format!("{}:{}", file, raw.line),
            }
        })
        .collect();

    Database {
        files: asm.files,
        warnings,
        classes: out_classes,
        excluded,
    }
}

/// A class's resolved base chain plus its merged keys, inputs and outputs (all indices into the
/// class list of `finish`).
type Resolved = (Vec<usize>, Vec<KeyDef>, Vec<IoDef>, Vec<IoDef>);

/// Resolve one class's base chain and merged keys/inputs/outputs (recursively, memoized).
fn resolve(
    idx: usize,
    classes: &[(RawClass, String)],
    by_name: &HashMap<String, usize>,
    resolved: &mut HashMap<usize, Resolved>,
    warnings: &mut Vec<String>,
) -> Resolved {
    if let Some(r) = resolved.get(&idx) {
        return r.clone();
    }
    // Cycle guard: a recursive base pair degenerates; insert an empty entry first.
    resolved.insert(idx, (Vec::new(), Vec::new(), Vec::new(), Vec::new()));
    let (raw, file) = &classes[idx];
    let mut chain: Vec<usize> = Vec::new();
    let mut keys: Vec<KeyDef> = Vec::new();
    let mut inputs: Vec<IoDef> = Vec::new();
    let mut outputs: Vec<IoDef> = Vec::new();
    for base in &raw.bases {
        let Some(&bidx) = by_name.get(&base.to_ascii_lowercase()) else {
            warnings.push(format!(
                "{file}:{}: base '{base}' of class '{}' not found",
                raw.line, raw.name
            ));
            continue;
        };
        if bidx == idx {
            continue; // a class deriving from itself
        }
        let (bchain, bkeys, binputs, boutputs) =
            resolve(bidx, classes, by_name, resolved, warnings).clone();
        // The chain is base-first: the base itself, then its own bases (deduplicated, first
        // mention wins) — `info_target base(Targetname, Parentname)` ->
        // [Targetname, GameEntity, Parentname].
        if !chain.contains(&bidx) {
            chain.push(bidx);
        }
        for c in bchain {
            if !chain.contains(&c) {
                chain.push(c);
            }
        }
        for k in bkeys {
            upsert_key(&mut keys, k);
        }
        for io in binputs {
            upsert_io(&mut inputs, io);
        }
        for io in boutputs {
            upsert_io(&mut outputs, io);
        }
    }
    for k in raw.keys.clone() {
        upsert_key(&mut keys, k);
    }
    for (name, type_, desc) in raw.inputs.clone() {
        upsert_io(
            &mut inputs,
            IoDef {
                name,
                type_,
                description: desc,
                declared_in: raw.name.clone(),
            },
        );
    }
    for (name, type_, desc) in raw.outputs.clone() {
        upsert_io(
            &mut outputs,
            IoDef {
                name,
                type_,
                description: desc,
                declared_in: raw.name.clone(),
            },
        );
    }
    let out = (chain, keys, inputs, outputs);
    resolved.insert(idx, out.clone());
    out
}

fn upsert_key(list: &mut Vec<KeyDef>, k: KeyDef) {
    let lower = k.name.to_ascii_lowercase();
    if let Some(pos) = list
        .iter()
        .position(|e| e.name.to_ascii_lowercase() == lower)
    {
        // `spawnflags` is the one key whose *value* merges: Hammer keeps the base class's flags
        // when a subclass adds its own (`trigger_push` declares a 128/256 block of its own and
        // still offers `TriggerOnce`'s 12 bits). Every other key, and a child's label/default for a
        // bit the base also declares, overrides in place.
        let mut merged = k;
        if lower == "spawnflags" && list[pos].type_ == "flags" && merged.type_ == "flags" {
            let base = std::mem::take(&mut list[pos].flags);
            merged.flags = merge_flag_defs(base, merged.flags);
        }
        list[pos] = merged;
    } else {
        list.push(k);
    }
}

/// A base class's declared flags plus a subclass's: a bit the base already declares is overridden
/// in place (label and default), a new bit is appended in the child's declaration order.
fn merge_flag_defs(mut base: Vec<Flag>, child: Vec<Flag>) -> Vec<Flag> {
    for flag in child {
        match base.iter_mut().find(|b| b.bit == flag.bit) {
            Some(existing) => *existing = flag,
            None => base.push(flag),
        }
    }
    base
}

fn upsert_io(list: &mut Vec<IoDef>, io: IoDef) {
    let lower = io.name.to_ascii_lowercase();
    if let Some(pos) = list.iter().position(|e| e.name.to_ascii_lowercase() == lower) {
        list[pos] = io;
    } else {
        list.push(io);
    }
}

/// In-place upsert of `items` into `list`: a later item whose key matches an existing entry
/// replaces it, a new one is appended (child overrides base in place).
fn upsert_by<T, K: Fn(&T) -> String>(list: &mut Vec<T>, items: impl IntoIterator<Item = T>, key_of: K) {
    for item in items {
        let key = key_of(&item);
        if let Some(pos) = list.iter().position(|e| key_of(e) == key) {
            list[pos] = item;
        } else {
            list.push(item);
        }
    }
}

/// `@OverrideClass X`: merge its keys/inputs/outputs/helpers into the class they override.
fn merge_override(target: &mut RawClass, ov: RawClass) {
    if ov.description.is_some() {
        target.description = ov.description.clone();
    }
    for b in ov.bases {
        if !target.bases.iter().any(|e| e.eq_ignore_ascii_case(&b)) {
            target.bases.push(b);
        }
    }
    target.helpers.extend(ov.helpers);
    for k in ov.keys {
        upsert_key(&mut target.keys, k);
    }
    upsert_by(&mut target.inputs, ov.inputs, |e| e.0.to_ascii_lowercase());
    upsert_by(&mut target.outputs, ov.outputs, |e| e.0.to_ascii_lowercase());
}

/// The process-wide database for `state.fgd_root`, cached by the file set + mtimes.
pub fn load_for(state: &HammerState) -> Result<Arc<Database>, FgdError> {
    static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = state.fgd_root.clone();
    if let Some(entry) = guard.as_ref() {
        if entry.root == root && fresh(&entry.mtimes) {
            return Ok(Arc::clone(&entry.db));
        }
    }
    let db = Arc::new(Database::load(&root)?);
    let mut mtimes = Vec::new();
    for f in &db.files {
        if let Ok(m) = std::fs::metadata(f).and_then(|m| m.modified()) {
            mtimes.push((f.clone(), m));
        }
    }
    *guard = Some(CacheEntry {
        root,
        mtimes,
        db: Arc::clone(&db),
    });
    Ok(db)
}

struct CacheEntry {
    root: PathBuf,
    /// (path, mtime) for every file in the cached database.
    mtimes: Vec<(PathBuf, std::time::SystemTime)>,
    db: Arc<Database>,
}

/// True when every recorded file still has its recorded mtime.
fn fresh(mtimes: &[(PathBuf, std::time::SystemTime)]) -> bool {
    mtimes.iter().all(|(path, m)| {
        std::fs::metadata(path)
            .and_then(|md| md.modified())
            .is_ok_and(|cur| cur == *m)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tags( … )` args recognize the three prop tags, case-insensitively and among other tag
    /// names (`tags( Info, PlayerPosition )` carries only those two).
    #[test]
    fn prop_tag_args() {
        assert_eq!(
            prop_tag_from_args("PropStatic").map(PropTag::as_str),
            Some("PropStatic")
        );
        assert_eq!(
            prop_tag_from_args("PropDynamic").map(PropTag::as_str),
            Some("PropDynamic")
        );
        assert_eq!(
            prop_tag_from_args("PropPhysics").map(PropTag::as_str),
            Some("PropPhysics")
        );
        assert_eq!(
            prop_tag_from_args("prop_physics").map(PropTag::as_str),
            None
        );
        assert_eq!(
            prop_tag_from_args("PROPdynamic").map(PropTag::as_str),
            Some("PropDynamic")
        );
        assert_eq!(
            prop_tag_from_args(" Info, PlayerPosition ").map(PropTag::as_str),
            None
        );
        assert_eq!(
            prop_tag_from_args("Info, PropStatic").map(PropTag::as_str),
            Some("PropStatic")
        );
        assert_eq!(prop_tag_from_args("").map(PropTag::as_str), None);
    }

    /// The trigger tag is recognized case-insensitively, anywhere in a multi-argument `tags( … )`,
    /// and never confused with a prop tag, another tag name or the `TriggerOnce` *class* name
    /// (which `is_trigger_class` matches separately).
    #[test]
    fn trigger_tag_args() {
        assert!(trigger_tag_from_args(" Trigger "));
        assert!(trigger_tag_from_args("TRIGGER"));
        assert!(trigger_tag_from_args("Info, Trigger"));
        assert!(trigger_tag_from_args("Info,Trigger,PlayerPosition"));
        assert!(!trigger_tag_from_args("TriggerOnce"));
        assert!(!trigger_tag_from_args("PropStatic"));
        assert!(!trigger_tag_from_args(" Sound "));
        assert!(!trigger_tag_from_args("Triggers"));
        assert!(!trigger_tag_from_args(""));
    }

    /// The `metadata { … }` body reader: `ident = value` pairs, quoted-string aware, and a key
    /// name that only appears inside a *value* does not match.
    #[test]
    fn metadata_body_values() {
        let body = "\n\t\tauto_apply_material = \"materials/tools/toolstrigger.vmat\"\n\
                    \t\tcollision_group = \"trigger\"\n\t\tphysics_only_model = true\n\t";
        assert_eq!(
            metadata_value(body, "auto_apply_material").as_deref(),
            Some("materials/tools/toolstrigger.vmat")
        );
        assert_eq!(
            metadata_value(body, "collision_group").as_deref(),
            Some("trigger")
        );
        assert_eq!(
            metadata_value(body, "AUTO_APPLY_MATERIAL").as_deref(),
            Some("materials/tools/toolstrigger.vmat")
        );
        assert_eq!(
            metadata_value(body, "physics_only_model").as_deref(),
            Some("true")
        );
        assert_eq!(metadata_value(body, "missing"), None);
        // A key name inside another value is skipped, not matched.
        assert_eq!(
            metadata_value(
                "entity_tool_name = \"auto_apply_material\"",
                "auto_apply_material"
            ),
            None
        );
        // Unterminated spans and degenerate bodies return None instead of panicking.
        assert_eq!(metadata_value("\"", "auto_apply_material"), None);
        assert_eq!(
            metadata_value("auto_apply_material =", "auto_apply_material"),
            None
        );
        assert_eq!(
            metadata_value("auto_apply_material", "auto_apply_material"),
            None
        );
        assert_eq!(metadata_value("", "auto_apply_material"), None);
        // Escaped quotes inside a value do not end it early.
        assert_eq!(
            metadata_value("a = \"x\\\"y\"", "a").as_deref(),
            Some("x\\\"y")
        );
    }

    /// The flag merge: the base's bits that the child does not mention stay, a shared bit takes the
    /// child's label and default, and a new bit is appended in the child's order.
    #[test]
    fn flag_defs_merge_by_bit() {
        let base = vec![
            Flag {
                bit: 1,
                label: "Clients".to_string(),
                default_on: true,
            },
            Flag {
                bit: 128,
                label: "Base only".to_string(),
                default_on: true,
            },
        ];
        let child = vec![
            Flag {
                bit: 128,
                label: "Once Only".to_string(),
                default_on: false,
            },
            Flag {
                bit: 256,
                label: "Added".to_string(),
                default_on: true,
            },
        ];
        let merged = merge_flag_defs(base, child);
        assert_eq!(
            merged.iter().map(|flag| flag.bit).collect::<Vec<_>>(),
            vec![1, 128, 256]
        );
        assert_eq!(merged[0].label, "Clients");
        assert!(merged[0].default_on);
        assert_eq!(
            merged[1].label, "Once Only",
            "the child's label wins for a shared bit"
        );
        assert!(!merged[1].default_on);
        assert_eq!(merged[2].label, "Added");
        // An empty child leaves the base untouched.
        let base = vec![Flag {
            bit: 2,
            label: "NPCs".to_string(),
            default_on: false,
        }];
        assert_eq!(merge_flag_defs(base, Vec::new()).len(), 1);
    }

    /// The project root's real game tree `<project root>/workshop/run/app/game` (the same
    /// walk-up `tests/fgd.rs` uses, so no host path is hard-coded).
    fn real_game_root() -> PathBuf {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        loop {
            let candidate = dir.join("workshop/run/app/game");
            if candidate.join("csgo/csgo.fgd").is_file() {
                return candidate;
            }
            assert!(
                dir.pop(),
                "no project root above the crate directory"
            );
        }
    }

    /// Corpus (FIX-V, review-rust.md B5): the SCOPE of the `spawnflags` merge on the real game
    /// tree. The merge — a subclass's own block adds to the base's instead of replacing it — is
    /// inferred, not read out of a live Hammer (`[I]`), so this counts exactly which classes it
    /// changes: for every real class whose own declaration carries `spawnflags(flags)`, its own
    /// bits (the raw parse) are compared with the resolved (merged) bits; `resolved ⊋ own` means a
    /// plain override would have dropped bits. Only classes with exactly one declaration are
    /// counted (the winner is unambiguous); several-declaration classes are reported separately.
    ///
    /// Run manually:
    /// `cargo test -p hammer-mcp --lib fgd::tests::real_game_spawnflags_merge_scope -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_game_spawnflags_merge_scope() {
        let root = real_game_root();
        let db = Database::load(&root).expect("the real tree parses without fatal errors");
        // every `spawnflags(flags)` a file declares, by class name (lowercased) — a class may be
        // declared more than once (`@exclude` + redefinition), so keep them all and pick the
        // unambiguous cases below.
        let mut own_by_class: HashMap<String, Vec<Vec<u64>>> = HashMap::new();
        for path in &db.files {
            let text = std::fs::read_to_string(path).expect("an FGD file is readable");
            let parsed = parser::parse_file(path, &text).expect("the file parses");
            for item in parsed.items {
                let ParsedItem::Class(raw) = item else {
                    continue;
                };
                let own: Option<Vec<u64>> = raw
                    .keys
                    .iter()
                    .find(|k| k.name.eq_ignore_ascii_case("spawnflags") && k.type_ == "flags")
                    .map(|k| {
                        let mut bits: Vec<u64> = k.flags.iter().map(|f| f.bit).collect();
                        bits.sort_unstable();
                        bits.dedup();
                        bits
                    });
                if let Some(bits) = own {
                    own_by_class
                        .entry(raw.name.to_ascii_lowercase())
                        .or_default()
                        .push(bits);
                }
            }
        }

        let mut changed: Vec<(String, Vec<u64>, Vec<u64>)> = Vec::new();
        let mut ambiguous: Vec<String> = Vec::new();
        for class in &db.classes {
            let Some(def) = db.spawnflags_def(&class.name) else {
                continue;
            };
            if !def.declared_in.eq_ignore_ascii_case(&class.name) {
                continue; // inherits the key untouched: a merge cannot change it
            }
            let Some(declarations) = own_by_class.get(&class.name.to_ascii_lowercase()) else {
                continue;
            };
            let mut resolved: Vec<u64> = def.flags.iter().map(|f| f.bit).collect();
            resolved.sort_unstable();
            resolved.dedup();
            // the winner is unambiguous when the class has exactly one declaration; with several
            // (`@exclude`+redefine) keep only the candidate that can be the winner (⊆ resolved)
            let own: Option<&Vec<u64>> = match declarations.as_slice() {
                [only] => Some(only),
                several => {
                    let candidates: Vec<&Vec<u64>> = several
                        .iter()
                        .filter(|bits| bits.iter().all(|b| resolved.contains(b)))
                        .collect();
                    match candidates.as_slice() {
                        [only] => Some(*only),
                        _ => {
                            ambiguous.push(class.name.clone());
                            None
                        }
                    }
                }
            };
            let Some(own) = own else { continue };
            assert!(
                own.iter().all(|b| resolved.contains(b)),
                "{}: the own bits {own:?} must be a subset of the resolved {resolved:?}",
                class.name
            );
            if own.as_slice() != resolved.as_slice() {
                changed.push((class.name.clone(), own.clone(), resolved));
            }
        }
        changed.sort_by(|a, b| a.0.cmp(&b.0));
        let declared = own_by_class
            .keys()
            .filter(|name| db.class(name).is_some())
            .count();
        println!(
            "real FGD spawnflags merge scope: {declared} classes declare spawnflags(flags); \
             {} change under the merge (own bits ⊊ resolved bits); ambiguous: {:?}",
            changed.len(),
            ambiguous
        );
        for (class, own, resolved) in &changed {
            println!("  {class}: own {own:?} -> resolved {resolved:?}");
        }
        assert!(declared > 0, "the real tree declares spawnflags somewhere");
        assert!(
            changed.iter().any(|(class, ..)| class == "trigger_push"),
            "the plan's merge case must be in the report: {changed:?}"
        );
    }
}
