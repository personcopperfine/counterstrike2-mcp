//! Particle (`.vpcf`) layer: a function-array model over [`cs2_kv3::Document`] with addressing,
//! edit ops, schema validation, templates and the content-path rules shared with `.vmdl`.
//!
//! A `.vpcf` is KV3 text (`cs2-kv3` parses and writes it byte-identically for Valve-authored
//! files). Unlike a `.vmdl` node tree the root object **is** the particle system, and its
//! function lists are nine fixed keys:
//!
//! ```text
//! <!-- kv3 … format:vpcf66 … -->
//! {
//!     _class = "CParticleSystemDefinition"
//!     m_nBehaviorVersion = 12
//!     m_nMaxParticles = 32
//!     m_Operators =
//!     [
//!         { _class = "C_OP_BasicMovement" },
//!     ]
//! }
//! ```
//!
//! * [`ARRAYS`] is the bridge type index <-> file key table ([`array_key`] / [`array_type`]);
//!   types `0..=6` are registry-backed **function** arrays (each entry's `_class` is a class the
//!   schema registers for that type), types `7`/`8` are **structural** arrays whose entries are
//!   plain structs (`ParticleChildrenInfo_t`, `ParticleNamedValueSource_t`) with an optional
//!   `_class` equal to the element class.
//! * [`FunctionRef`] addresses one entry (`"m_Operators/2"`); `"/"` is the root.
//! * [`namedvalue`] knows the `m_NamedValueLocals` entry form (as pet writes it, seen live 2026-10-13) and
//!   [`Particle::validate`] runs its entry checks after the array loop.
//! * [`Particle`] parses, lists ([`Particle::list`]), renders ([`Particle::function_json`],
//!   [`Particle::root_json`]) and edits ([`Particle::apply`], atomic: all-or-nothing).
//! * [`Schema`] is the `research/pet-schema.json` snapshot; [`Particle::validate`] checks a file
//!   against it (structure + registry + fields), [`Particle::validate_strict`] returns the first
//!   error.
//! * [`template`] builds a fresh minimal system: `fixtures/vpcf/probe.vpcf` plus the
//!   `m_nBehaviorVersion` root field = [`CURRENT_BEHAVIOR_VERSION`] (the probe itself is the
//!   live-compiled reference file and predates the field).
//!
//! Conventions (matching the ModelDoc layer, verified against the live probe file):
//!
//! * [`Particle::write`] is [`cs2_kv3::write`] — Valve's layout (CRLF, no final newline) — so a
//!   file written by `particle_create`/`particle_edit` looks like every other KV3 file the repo
//!   produces. A hand-written `.vpcf` (the live probe is LF with a trailing newline) parses and
//!   re-serialises semantically intact, not byte-for-byte; see `tests/vpcf.rs`
//!   (`live_probe_and_fixture_differ_only_in_line_endings`).
//! * object key order is *not* preserved by a JSON round trip: `serde_json` maps are
//!   alphabetical, so an entry inserted from JSON gets alphabetical field order after `_class`
//!   (the same limitation as [`crate::vmdl`]).
//! * flag and blob values convert through [`cs2_kv3::from_json`] / [`cs2_kv3::to_json`],
//!   exactly as the ModelDoc layer does (`resource_name:"x"` is
//!   `{"$flag":"resource_name","value":"x"}`).

pub mod namedvalue;
pub mod ops;
pub mod schema;
pub mod template;

pub use ops::{Op, OpResult};
pub use schema::{known_schema, ClassField, ClassInfo, FunctionRow, Schema};
pub use template::{template, TEMPLATE_CLASSES};

use cs2_kv3::{Object, Value};
use serde_json::{json, Map, Value as Json};
use std::fmt;
use std::str::FromStr;

/// The KV3 `format:` id every `.vpcf` carries.
pub const FORMAT: &str = "vpcf66";

/// The root `_class` of a `.vpcf`; anything else is not a particle system.
pub const ROOT_CLASS: &str = "CParticleSystemDefinition";

/// The root field pet reads a system's behavior version from (`CParticleSystemDefinition`'s
/// `m_nBehaviorVersion`, int32 — see [`CURRENT_BEHAVIOR_VERSION`] for why it matters and
/// [`Particle::validate_structure`] for the check).
pub const BEHAVIOR_VERSION_FIELD: &str = "m_nBehaviorVersion";

/// The behavior version the current build's particle editor (pet) treats as up to date
/// ("fact A", live finding 2026-10-13): a `.vpcf` whose root carries no `m_nBehaviorVersion`
/// makes pet show the red banner *"This particle system is using old behavior version 0, while
/// current version is 12!"* (a missing field reads as 0), while `resourcecompiler` still compiles
/// the file. The [`template`] writes the field so fresh `.vpcf`s never see the banner, and the
/// validator reports a missing/stale value as a **warning** and a non-integer one as an error.
pub const CURRENT_BEHAVIOR_VERSION: i64 = 12;

/// The header line of a new `.vpcf` (the header is text; the GUIDs are the ones `pet` passes to
/// `LoadKV3`/`SaveKV3`, read from the live probe file).
pub const HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->";

/// The nine function arrays: `(bridge type index, file key, element class)`.
///
/// Types `0..=6` are the registry-backed function arrays (element class `None`: their entries are
/// `_class`-named particle functions, resolved through [`Schema`]); types `7`/`8` are the
/// structural arrays whose entries are plain structs. This table is the single mapping between
/// the bridge's integer type index and the file key, identical on both sides.
pub const ARRAYS: [(usize, &str, Option<&str>); 9] = [
    (0, "m_PreEmissionOperators", None),
    (1, "m_Emitters", None),
    (2, "m_Initializers", None),
    (3, "m_Operators", None),
    (4, "m_ForceGenerators", None),
    (5, "m_Constraints", None),
    (6, "m_Renderers", None),
    (7, "m_Children", Some("ParticleChildrenInfo_t")),
    (8, "m_NamedValueLocals", Some("ParticleNamedValueSource_t")),
];

/// The file key of bridge type `type` (`0..=8`), `None` outside that range.
pub fn array_key(r#type: usize) -> Option<&'static str> {
    ARRAYS.get(r#type).map(|(_, key, _)| *key)
}

/// The bridge type index of a file key, `None` when `key` is not one of the nine arrays.
pub fn array_type(key: &str) -> Option<usize> {
    ARRAYS
        .iter()
        .find(|(_, candidate, _)| *candidate == key)
        .map(|(r#type, _, _)| *r#type)
}

/// The struct class of a structural array (types `7`/`8`), `None` for the function arrays.
pub fn array_element_class(key: &str) -> Option<&'static str> {
    ARRAYS
        .iter()
        .find(|(_, candidate, _)| *candidate == key)
        .and_then(|(_, _, element)| *element)
}

/// The nine file keys, in type order (for messages).
pub fn array_keys() -> Vec<&'static str> {
    ARRAYS.iter().map(|(_, key, _)| *key).collect()
}

/// An address of one entry: `"m_Operators/2"`. `"/"` (an empty `array`) is the root object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct FunctionRef {
    /// The array key; empty for the root.
    pub array: String,
    /// The entry index; always 0 for the root.
    pub index: usize,
}

impl FunctionRef {
    /// The root object (`"/"`).
    pub fn root() -> FunctionRef {
        FunctionRef::default()
    }

    /// The entry `index` of `array`.
    pub fn new(array: impl Into<String>, index: usize) -> FunctionRef {
        FunctionRef {
            array: array.into(),
            index,
        }
    }

    /// True for the root (`"/"`).
    pub fn is_root(&self) -> bool {
        self.array.is_empty()
    }
}

impl fmt::Display for FunctionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            return f.write_str("/");
        }
        write!(f, "{}/{}", self.array, self.index)
    }
}

impl FromStr for FunctionRef {
    type Err = VpcfError;

    /// `"/"` (the root), `"m_Operators/2"`; the surrounding whitespace is ignored.
    fn from_str(s: &str) -> Result<FunctionRef, VpcfError> {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed == "/" {
            return Ok(FunctionRef::root());
        }
        let (array, index) = trimmed.split_once('/').ok_or_else(|| {
            VpcfError::Path(format!(
                "bad function reference {s:?}: expected \"<array>/<index>\" or \"/\""
            ))
        })?;
        if array.is_empty() {
            return Err(VpcfError::Path(format!(
                "bad function reference {s:?}: the array key is empty"
            )));
        }
        let index = index.parse::<usize>().map_err(|_| {
            VpcfError::Path(format!(
                "bad function reference {s:?}: {index:?} is not an index"
            ))
        })?;
        Ok(FunctionRef::new(array, index))
    }
}

/// One entry as [`Particle::list`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    /// The array key the entry lives in.
    pub array: String,
    /// The bridge type index of that array (`0..=8`).
    pub r#type: usize,
    /// The entry's index inside the array.
    pub index: usize,
    /// The entry's `_class`; `None` for a `_class`-less entry (types 7/8).
    pub class: Option<String>,
    /// The entry's field keys, `_class` excluded, in file order.
    pub field_keys: Vec<String>,
}

/// Severity of one [`Issue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

impl Level {
    pub fn is_error(self) -> bool {
        matches!(self, Level::Error)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Error => "error",
            Level::Warning => "warning",
        })
    }
}

/// One problem [`Particle::validate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// `"/"` for the file, an array key for an array, `m_Operators/2` for one entry, the key
    /// itself for an unknown root field.
    pub path: String,
    pub level: Level,
    pub msg: String,
}

impl Issue {
    pub fn new(level: Level, path: impl Into<String>, msg: impl Into<String>) -> Issue {
        Issue {
            path: path.into(),
            level,
            msg: msg.into(),
        }
    }

    pub fn error(path: impl Into<String>, msg: impl Into<String>) -> Issue {
        Issue::new(Level::Error, path, msg)
    }

    pub fn warning(path: impl Into<String>, msg: impl Into<String>) -> Issue {
        Issue::new(Level::Warning, path, msg)
    }
}

/// [`Particle::validate`] options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidateOptions {
    /// An obsolete class (`MObsoleteParticleFunction` in the schema) is a warning when true (the
    /// `validate` default) and an error when false (what [`Particle::validate_strict`] passes).
    pub allow_obsolete: bool,
}

impl Default for ValidateOptions {
    fn default() -> Self {
        ValidateOptions {
            allow_obsolete: true,
        }
    }
}

/// Everything the particle layer can fail with.
#[derive(Debug)]
pub enum VpcfError {
    /// `cs2-kv3` could not parse the text.
    Parse(cs2_kv3::ParseError),
    /// Validation found errors (also used for bad [`template`] arguments, with `path` = `"/"`).
    Invalid(Vec<Issue>),
    /// `array` is not one of the nine function arrays.
    UnknownArray(String),
    /// There is no entry at that reference (`"m_Operators/9"`).
    FunctionNotFound(String),
    /// An edit op was rejected. `index` is its position in the `ops` array.
    BadOp { index: usize, msg: String },
    /// The schema document is malformed.
    Schema(String),
    /// The schema is the committed placeholder (or has `placeholder: true`): it has no classes,
    /// so every validation would fail; callers surface this as `SchemaUnavailable`.
    SchemaPlaceholder,
    /// A file-system problem while reading a schema or a `.vpcf`.
    Io(std::io::Error),
    /// A path/reference argument that cannot be resolved (bad shape, `..`, not mapped, …).
    Path(String),
}

impl fmt::Display for VpcfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VpcfError::Parse(error) => write!(f, "parse error: {error}"),
            VpcfError::Invalid(issues) => match issues.first() {
                Some(first) => write!(
                    f,
                    "invalid particle: {} issue(s), first at {}: {}",
                    issues.len(),
                    first.path,
                    first.msg
                ),
                None => f.write_str("invalid particle"),
            },
            VpcfError::UnknownArray(array) => write!(f, "{array:?} is not a function array"),
            VpcfError::FunctionNotFound(reference) => {
                write!(f, "no function at {reference}")
            }
            VpcfError::BadOp { index, msg } => write!(f, "op[{index}]: {msg}"),
            VpcfError::Schema(msg) => write!(f, "bad schema: {msg}"),
            VpcfError::SchemaPlaceholder => f.write_str(
                "the particle schema is still the committed placeholder (no classes); \
                 run the live snapshot step",
            ),
            VpcfError::Io(error) => write!(f, "{error}"),
            VpcfError::Path(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for VpcfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VpcfError::Parse(error) => Some(error),
            VpcfError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<cs2_kv3::ParseError> for VpcfError {
    fn from(error: cs2_kv3::ParseError) -> Self {
        VpcfError::Parse(error)
    }
}

impl From<std::io::Error> for VpcfError {
    fn from(error: std::io::Error) -> Self {
        VpcfError::Io(error)
    }
}

/// A parsed `.vpcf`. [`Particle::parse`] only checks that the text is KV3: a wrong root `_class`
/// or a malformed array is reported by [`Particle::validate_structure`] /
/// [`Particle::validate`], so the `particle_validate` tool can answer with `ok:false` and the
/// issues instead of failing on the read.
#[derive(Debug, Clone)]
pub struct Particle {
    pub doc: cs2_kv3::Document,
}

impl Particle {
    /// Parse the KV3 text of a `.vpcf`.
    pub fn parse(text: &str) -> Result<Particle, VpcfError> {
        Ok(Particle {
            doc: cs2_kv3::parse(text)?,
        })
    }

    /// The file text: [`cs2_kv3::write`] — Valve's layout (CRLF, no final newline).
    pub fn write(&self) -> String {
        cs2_kv3::write(&self.doc)
    }

    /// The root object (in a `.vpcf` that *is* the `CParticleSystemDefinition`, unlike `.vmdl`'s
    /// `rootNode`).
    pub fn root(&self) -> &Object {
        &self.doc.root
    }

    /// The root object, mutably.
    pub fn root_mut(&mut self) -> &mut Object {
        &mut self.doc.root
    }

    /// Every entry of every array (or of `array` alone), optionally filtered to one `_class`.
    ///
    /// Entries in the order of [`ARRAYS`], then file order. `class: None` on an entry means that
    /// entry has no `_class` (normal in the structural arrays 7/8; an error in 0..=6, reported by
    /// validation). Filtering never errors: an unknown array key simply yields no entries.
    pub fn list(&self, array: Option<&str>, class: Option<&str>) -> Vec<FunctionInfo> {
        let mut out = Vec::new();
        for (r#type, key, _) in ARRAYS {
            if array.is_some_and(|wanted| wanted != key) {
                continue;
            }
            let Some(items) = self.doc.root.get(key).and_then(Value::as_array) else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let Some(entry) = item.as_object() else {
                    continue;
                };
                let entry_class = entry.get("_class").and_then(Value::as_str);
                if class.is_some_and(|wanted| entry_class != Some(wanted)) {
                    continue;
                }
                out.push(FunctionInfo {
                    array: key.to_string(),
                    r#type,
                    index,
                    class: entry_class.map(str::to_string),
                    field_keys: entry
                        .keys()
                        .filter(|field| *field != "_class")
                        .map(str::to_string)
                        .collect(),
                });
            }
        }
        out
    }

    /// The object at `function` — the root for `"/"`.
    pub fn function(&self, function: &FunctionRef) -> Result<&Object, VpcfError> {
        if function.is_root() {
            return Ok(&self.doc.root);
        }
        if array_type(&function.array).is_none() {
            return Err(VpcfError::UnknownArray(function.array.clone()));
        }
        self.doc
            .root
            .get(&function.array)
            .and_then(Value::as_array)
            .and_then(|items| items.get(function.index))
            .and_then(Value::as_object)
            .ok_or_else(|| VpcfError::FunctionNotFound(function.to_string()))
    }

    /// One entry as JSON: `{"path", "array", "type", "index", "_class", "fields"}`.
    ///
    /// `path` is the reference string (`"m_Operators/2"`, `"/"` for the root), `fields` holds
    /// every key except `_class` (for the root that includes the arrays), values converted with
    /// [`cs2_kv3::to_json`]. `array` / `type` / `index` are `null` for the root.
    pub fn function_json(&self, function: &FunctionRef) -> Result<Json, VpcfError> {
        let object = self.function(function)?;
        let mut fields = Map::new();
        for (key, value) in object.iter() {
            if key == "_class" {
                continue;
            }
            fields.insert(key.to_string(), cs2_kv3::to_json(value));
        }
        let roots = function.is_root();
        Ok(json!({
            "path": function.to_string(),
            "array": if roots { Json::Null } else { json!(function.array) },
            "type": if roots { Json::Null } else { json!(array_type(&function.array)) },
            "index": if roots { Json::Null } else { json!(function.index) },
            "_class": object.get("_class").and_then(Value::as_str).unwrap_or_default(),
            "fields": Json::Object(fields),
        }))
    }

    /// The root object as JSON. With `include_arrays` the nine array keys keep their entries;
    /// without it each *present* array key is replaced, in place, by its entry count.
    pub fn root_json(&self, include_arrays: bool) -> Json {
        let mut out = Map::new();
        for (key, value) in self.doc.root.iter() {
            if !include_arrays && array_type(key).is_some() {
                let count = value.as_array().map(<[Value]>::len).unwrap_or(0);
                out.insert(key.to_string(), json!(count));
            } else {
                out.insert(key.to_string(), cs2_kv3::to_json(value));
            }
        }
        Json::Object(out)
    }

    /// Apply `ops` in order. **All-or-nothing**: the ops run against a clone of the document and
    /// the clone replaces it only when every op succeeded, so a failure leaves the particle
    /// exactly as it was.
    pub fn apply(&mut self, ops: &[Op]) -> Result<Vec<OpResult>, VpcfError> {
        let mut working = self.doc.clone();
        let results = ops::apply_ops(&mut working, ops)?;
        self.doc = working;
        Ok(results)
    }

    /// Structural problems, with no schema needed: a non-`vpcf66` header, a root `_class` that
    /// is not [`ROOT_CLASS`], a missing/stale/non-integer root `m_nBehaviorVersion`
    /// ([`CURRENT_BEHAVIOR_VERSION`]: the behavior-version warning pet shows its red banner
    /// for), an array value that is not an array of objects, and a missing or non-string
    /// `_class` on an entry of the function arrays (0..=6).
    pub fn validate_structure(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.doc.header.format != FORMAT {
            issues.push(Issue::error(
                "/",
                format!(
                    "header format {:?} is not {FORMAT:?}",
                    self.doc.header.format
                ),
            ));
        }
        match self.doc.root.get("_class") {
            Some(Value::String(class)) if class == ROOT_CLASS => {}
            Some(Value::String(class)) => issues.push(Issue::error(
                "/",
                format!("root _class is {class:?}, expected {ROOT_CLASS:?}"),
            )),
            Some(_) => issues.push(Issue::error("/", "_class is not a string")),
            None => issues.push(Issue::error(
                "/",
                format!("root _class is missing (expected {ROOT_CLASS:?})"),
            )),
        }
        // Fact A: pet greets a file whose root carries no `m_nBehaviorVersion` (read as 0), or an
        // older one, with the red "old behavior version" banner; `resourcecompiler` still compiles
        // such a file, so this is a warning and never a refusal — only a non-integer value, which
        // the int32 field cannot hold, is an error. A value >= [`CURRENT_BEHAVIOR_VERSION`] is
        // accepted silently.
        match self.doc.root.get(BEHAVIOR_VERSION_FIELD) {
            Some(Value::Number(number)) => match number.as_i64() {
                Some(found) if found >= CURRENT_BEHAVIOR_VERSION => {}
                Some(found) => issues.push(Issue::warning(
                    BEHAVIOR_VERSION_FIELD,
                    format!(
                        "m_nBehaviorVersion is {found} while the current version is \
                         {CURRENT_BEHAVIOR_VERSION}: pet will show the \
                         \"old behavior version\" banner for this file"
                    ),
                )),
                None => issues.push(Issue::error(
                    BEHAVIOR_VERSION_FIELD,
                    format!(
                        "m_nBehaviorVersion is {number}, not an integer (the field is a \
                         CParticleSystemDefinition int32)"
                    ),
                )),
            },
            Some(other) => issues.push(Issue::error(
                BEHAVIOR_VERSION_FIELD,
                format!(
                    "m_nBehaviorVersion is not an integer (the field is a \
                     CParticleSystemDefinition int32), got {other:?}"
                ),
            )),
            None => issues.push(Issue::warning(
                BEHAVIOR_VERSION_FIELD,
                format!(
                    "m_nBehaviorVersion is missing (pet reads it as 0) while the current version \
                     is {CURRENT_BEHAVIOR_VERSION}: pet will show the \
                     \"old behavior version\" banner for this file"
                ),
            )),
        }
        for (r#type, key, _) in ARRAYS {
            let Some(value) = self.doc.root.get(key) else {
                continue;
            };
            let Some(items) = value.as_array() else {
                issues.push(Issue::error(key, format!("{key} is not an array of objects")));
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let path = format!("{key}/{index}");
                let Some(entry) = item.as_object() else {
                    issues.push(Issue::error(&path, "entry is not an object"));
                    continue;
                };
                if r#type <= 6 {
                    match entry.get("_class") {
                        Some(Value::String(class)) if !class.is_empty() => {}
                        Some(Value::String(_)) => {
                            issues.push(Issue::error(&path, "_class is empty"))
                        }
                        Some(_) => issues.push(Issue::error(&path, "_class is not a string")),
                        None => issues.push(Issue::error(
                            &path,
                            format!("_class is missing ({key} entries need one)"),
                        )),
                    }
                }
            }
        }
        issues
    }

    /// Structural problems (as [`Particle::validate_structure`]) plus the schema checks:
    ///
    /// * every root field must exist on the schema's root class (the array keys and `_class` are
    ///   structure);
    /// * a function entry's `_class` must be a class the schema registers for that array
    ///   (otherwise the message names the arrays it *is* registered for);
    /// * every entry field must exist on that class's **flattened** field list (base-class fields
    ///   included; `_class` exempt);
    /// * a type 7/8 entry's `_class`, when present, must be the array's element class, and its
    ///   fields must exist on that struct class;
    /// * the `m_NamedValueLocals` (type 8) entries get [`namedvalue::validate_entries`]: a
    ///   non-empty unique `m_Name`, a `PVAL_*` `m_ValueType` and `m_DefaultConfig` keys that exist
    ///   on `ParticleNamedValueConfiguration_t`;
    /// * an obsolete class is a warning when `opts.allow_obsolete`, an error otherwise.
    ///
    /// A schema that lacks the root class or a structural element class is an error at that path
    /// (fail closed: an incomplete snapshot must never look like a clean file).
    pub fn validate(&self, schema: &Schema, opts: ValidateOptions) -> Vec<Issue> {
        let mut issues = self.validate_structure();

        match schema.class(&schema.root_class) {
            Some(info) => {
                for (key, _) in self.doc.root.iter() {
                    // The behavior version is checked (as a field of the root) by
                    // [`Particle::validate_structure`]'s dedicated rule, which runs with or
                    // without the snapshot, so the root-field list must not flag it — some
                    // snapshots may not carry this field yet.
                    if key == "_class" || key == BEHAVIOR_VERSION_FIELD || array_type(key).is_some() {
                        continue;
                    }
                    if !info.has_field(key) {
                        issues.push(Issue::error(
                            key,
                            format!(
                                "unknown root field {key:?} (not a field of {})",
                                schema.root_class
                            ),
                        ));
                    }
                }
            }
            None => issues.push(Issue::error(
                "/",
                format!(
                    "schema has no class {:?} to check the root against",
                    schema.root_class
                ),
            )),
        }

        for (r#type, key, element) in ARRAYS {
            let Some(items) = self.doc.root.get(key).and_then(Value::as_array) else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                let Some(entry) = item.as_object() else {
                    continue;
                };
                let path = format!("{key}/{index}");
                let entry_class = entry.get("_class").and_then(Value::as_str);
                if r#type <= 6 {
                    // A missing/non-string `_class` is already a structural error.
                    let Some(class) = entry_class.filter(|class| !class.is_empty()) else {
                        continue;
                    };
                    let Some(info) = schema.class(class) else {
                        issues.push(Issue::error(
                            &path,
                            format!("unknown function class {class:?} (not in the schema)"),
                        ));
                        continue;
                    };
                    let registered = schema.registration_types(class);
                    if !registered.contains(&r#type) {
                        if registered.is_empty() {
                            issues.push(Issue::error(
                                &path,
                                format!("class {class:?} is not registered for any function array"),
                            ));
                        } else {
                            let expected: Vec<&str> =
                                registered.iter().filter_map(|t| array_key(*t)).collect();
                            issues.push(Issue::error(
                                &path,
                                format!(
                                    "class {class:?} is not registered for {key} \
                                     (expected one of: {})",
                                    expected.join(", ")
                                ),
                            ));
                        }
                    } else if schema.is_obsolete(r#type, class) {
                        let level = if opts.allow_obsolete {
                            Level::Warning
                        } else {
                            Level::Error
                        };
                        issues.push(Issue::new(
                            level,
                            &path,
                            format!("class {class:?} is obsolete (MObsoleteParticleFunction)"),
                        ));
                    }
                    for field in entry.keys() {
                        if field == "_class" {
                            continue;
                        }
                        if !info.has_field(field) {
                            issues.push(Issue::error(
                                &path,
                                format!("unknown field {field:?} on class {class:?}"),
                            ));
                        }
                    }
                } else {
                    let Some(element) = element else { continue };
                    if let Some(class) = entry_class {
                        if class != element {
                            issues.push(Issue::error(
                                &path,
                                format!("_class {class:?} is not the element class {element:?}"),
                            ));
                            continue;
                        }
                    }
                    let Some(info) = schema.class(element) else {
                        issues.push(Issue::error(
                            &path,
                            format!(
                                "schema has no class {element:?} to check {key} entries against"
                            ),
                        ));
                        continue;
                    };
                    for field in entry.keys() {
                        // `_class` equal to the element class is structure, not a field.
                        if field == "_class" {
                            continue;
                        }
                        if !info.has_field(field) {
                            issues.push(Issue::error(
                                &path,
                                format!("unknown field {field:?} on class {element:?}"),
                            ));
                        }
                    }
                }
            }
        }

        // The `m_NamedValueLocals` (type 8) entries: the structural loop above checked their
        // fields against `ParticleNamedValueSource_t`; [`namedvalue`] checks the entry's own
        // value (`m_Name` uniqueness, the `PVAL_*` type, the `m_DefaultConfig` keys).
        issues.extend(namedvalue::validate_entries(&self.doc.root, Some(schema)));

        issues
    }

    /// [`Particle::validate`] reduced to its verdict: the first **error**, or `Ok(())`. Warnings
    /// never fail. `allow_obsolete` is passed through to [`ValidateOptions::allow_obsolete`], so
    /// `false` turns an obsolete class into a failing error.
    pub fn validate_strict(
        &self,
        schema: &Schema,
        allow_obsolete: bool,
    ) -> Result<(), VpcfError> {
        let mut issues = self.validate(schema, ValidateOptions { allow_obsolete });
        match issues.iter().position(|issue| issue.level.is_error()) {
            Some(first) => Err(VpcfError::Invalid(vec![issues.swap_remove(first)])),
            None => Ok(()),
        }
    }
}
