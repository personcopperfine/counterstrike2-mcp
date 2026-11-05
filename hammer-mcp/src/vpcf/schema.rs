//! The particle schema snapshot (`research/pet-schema.json`) and its loader.
//!
//! The snapshot is the authoritative offline description of what a `.vpcf` may contain: the nine
//! arrays, every function class and the array types it is registered for, and every class's
//! **flattened** field list (base-class fields included, each tagged with `declaredIn`). It is
//! written by the live step (`test_pet.py --write-snapshot`, `particle_types {writeSnapshot:true}`)
//! and committed; the committed version is the **live snapshot taken 2026-10-13 on build
//! 2000930** (not a placeholder), so the offline validator checks against live pet data. A
//! snapshot still loaded with `placeholder: true` fails with [`VpcfError::SchemaPlaceholder`]
//! so a validator can never pass against an empty registry.
//!
//! [`Schema::from_json`] is strict about shape (a malformed snapshot must not silently become an
//! empty one) but lenient about absence: a missing `arrays` / `functions` / `classes` is an empty
//! list, while a wrong *type* is an error.

use crate::vpcf::{VpcfError, ROOT_CLASS};
use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::path::Path;

/// The snapshot committed with this crate, read by the validator when no newer one is on disk.
pub const BUILTIN_SNAPSHOT: &str = include_str!("../../research/pet-schema.json");

/// One row of the snapshot's `arrays` list (`{"type":0,"key":"…","elementClass":null}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayRow {
    pub r#type: usize,
    pub key: String,
    pub element_class: Option<String>,
}

/// One row of the snapshot's `functions` list: a class registered for one array type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionRow {
    pub r#type: usize,
    pub class_name: String,
    /// The editor's friendly name (unused by validation).
    pub name: String,
    /// The class carries the `MObsoleteParticleFunction` static metadata.
    pub obsolete: bool,
    /// "Only one allowed" (the game's group `-1`); always `null` this pass.
    pub singleton: Option<bool>,
}

/// One field of a class (`{"name","type","declaredIn"}`). `type` is the declared type name or
/// `None` when the live walk could not read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassField {
    pub name: String,
    pub r#type: Option<String>,
    pub declared_in: String,
}

/// One class: its base chain, its **flattened** fields and its static metadata names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassInfo {
    pub bases: Vec<String>,
    pub fields: Vec<ClassField>,
    pub metadata: Vec<String>,
}

impl ClassInfo {
    /// True when `field` is one of the class's flattened fields.
    pub fn has_field(&self, field: &str) -> bool {
        self.fields.iter().any(|candidate| candidate.name == field)
    }
}

/// A loaded, non-placeholder schema snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// The game build the snapshot was taken from, when it records one.
    pub build: Option<String>,
    /// The root class the snapshot's files use (defaults to [`ROOT_CLASS`]).
    pub root_class: String,
    pub arrays: Vec<ArrayRow>,
    pub functions: Vec<FunctionRow>,
    pub classes: BTreeMap<String, ClassInfo>,
}

impl Schema {
    /// Parse a snapshot document. `placeholder: true` is rejected with
    /// [`VpcfError::SchemaPlaceholder`] (the committed stub carries no classes, so validating
    /// against it could only produce nonsense).
    pub fn from_json(json: &Json) -> Result<Schema, VpcfError> {
        let map = json
            .as_object()
            .ok_or_else(|| schema_error("the schema must be a JSON object"))?;
        if map.get("placeholder").and_then(Json::as_bool) == Some(true) {
            return Err(VpcfError::SchemaPlaceholder);
        }
        let build = match map.get("build") {
            None | Some(Json::Null) => None,
            Some(Json::String(text)) => Some(text.clone()),
            Some(other) => {
                return Err(schema_error(format!(
                    "build must be a string or null, got {other}"
                )))
            }
        };
        let root_class = match map.get("rootClass") {
            None | Some(Json::Null) => ROOT_CLASS.to_string(),
            Some(Json::String(text)) if !text.is_empty() => text.clone(),
            Some(other) => {
                return Err(schema_error(format!(
                    "rootClass must be a non-empty string, got {other}"
                )))
            }
        };
        Ok(Schema {
            build,
            root_class,
            arrays: parse_arrays(map.get("arrays"))?,
            functions: parse_functions(map.get("functions"))?,
            classes: parse_classes(map.get("classes"))?,
        })
    }

    /// Read and parse a snapshot file.
    pub fn load(path: &Path) -> Result<Schema, VpcfError> {
        let text = std::fs::read_to_string(path)?;
        let json: Json = serde_json::from_str(&text)
            .map_err(|error| schema_error(format!("{}: {error}", path.display())))?;
        Schema::from_json(&json)
    }

    /// Parse the snapshot committed with the crate ([`BUILTIN_SNAPSHOT`]): the live snapshot of
    /// 2026-10-13 (build 2000930). A `placeholder:true` document is [`VpcfError::SchemaPlaceholder`].
    pub fn builtin() -> Result<Schema, VpcfError> {
        let json: Json = serde_json::from_str(BUILTIN_SNAPSHOT).map_err(|error| {
            schema_error(format!("the built-in pet-schema.json is not JSON: {error}"))
        })?;
        Schema::from_json(&json)
    }

    /// The class named `name`, when the snapshot has it.
    pub fn class(&self, name: &str) -> Option<&ClassInfo> {
        self.classes.get(name)
    }

    /// Every array type `class_name` is registered for, sorted, without duplicates.
    pub fn registration_types(&self, class_name: &str) -> Vec<usize> {
        let mut types: Vec<usize> = self
            .functions
            .iter()
            .filter(|row| row.class_name == class_name)
            .map(|row| row.r#type)
            .collect();
        types.sort_unstable();
        types.dedup();
        types
    }

    /// True when `class_name` is registered for `r#type` **and** that registration is obsolete.
    pub fn is_obsolete(&self, r#type: usize, class_name: &str) -> bool {
        self.functions
            .iter()
            .any(|row| row.r#type == r#type && row.class_name == class_name && row.obsolete)
    }
}

/// The schema to validate against: the snapshot at `path` when it loads, else the built-in one.
/// The second value names the source that was used (`path`, `"built-in (build <n>)"`,
/// `"built-in (placeholder)"`), for the tool result's `schemaSource`.
///
/// The *only* fallbacks are a snapshot that **does not exist** (io `NotFound` on any component of
/// the path) and the committed **placeholder** (`placeholder:true`, no classes): both are "no
/// newer snapshot here" states. Any other load error — JSON syntax, a wrong shape, a permission
/// problem — means a snapshot exists but is unusable, and falling back to the built-in would
/// silently validate against stale data under the caller's nose (e.g. a half-written or
/// hand-corrupted `research/pet-schema.json`). Those are returned as the error, with `source`
/// naming the file.
///
/// Mirrors [`crate::vmdl::known_classes`]: a newer snapshot in the project tree wins, the
/// committed one is the fallback that always works from any install location.
pub fn known_schema(path: &Path) -> (Result<Schema, VpcfError>, String) {
    match Schema::load(path) {
        Ok(schema) => (Ok(schema), path.display().to_string()),
        // genuinely absent: like the missing file the built-in is the fallback that always works
        Err(VpcfError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => fallback(),
        Err(VpcfError::SchemaPlaceholder) => fallback(),
        other => (other, path.display().to_string()),
    }
}

/// Fall back to the built-in snapshot (source `"built-in (build <n>)"`, or
/// `"built-in (placeholder)"` while the committed stub is not a real one).
fn fallback() -> (Result<Schema, VpcfError>, String) {
    match Schema::builtin() {
        Ok(schema) => {
            let build = schema.build.clone().unwrap_or_else(|| "?".to_string());
            (Ok(schema), format!("built-in (build {build})"))
        }
        Err(VpcfError::SchemaPlaceholder) => (Err(VpcfError::SchemaPlaceholder), "built-in (placeholder)".to_string()),
        Err(error) => (Err(error), "built-in (unavailable)".to_string()),
    }
}

fn schema_error(msg: impl Into<String>) -> VpcfError {
    VpcfError::Schema(msg.into())
}

fn parse_arrays(value: Option<&Json>) -> Result<Vec<ArrayRow>, VpcfError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let rows = value
        .as_array()
        .ok_or_else(|| schema_error(format!("arrays must be an array, got {value}")))?;
    rows.iter()
        .map(|row| {
            let row = row
                .as_object()
                .ok_or_else(|| schema_error(format!("an arrays entry is not an object: {row}")))?;
            Ok(ArrayRow {
                r#type: integer(row, "type")?,
                key: string(row, "key")?,
                element_class: optional_string(row.get("elementClass"))?,
            })
        })
        .collect()
}

fn parse_functions(value: Option<&Json>) -> Result<Vec<FunctionRow>, VpcfError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let rows = value
        .as_array()
        .ok_or_else(|| schema_error(format!("functions must be an array, got {value}")))?;
    rows.iter()
        .map(|row| {
            let row = row.as_object().ok_or_else(|| {
                schema_error(format!("a functions entry is not an object: {row}"))
            })?;
            Ok(FunctionRow {
                r#type: integer(row, "type")?,
                class_name: string(row, "className")?,
                name: optional_string(row.get("name"))?.unwrap_or_default(),
                obsolete: match row.get("obsolete") {
                    None | Some(Json::Null) => false,
                    Some(Json::Bool(value)) => *value,
                    Some(other) => {
                        return Err(schema_error(format!(
                            "functions[].obsolete must be a bool, got {other}"
                        )))
                    }
                },
                singleton: match row.get("singleton") {
                    None | Some(Json::Null) => None,
                    Some(Json::Bool(value)) => Some(*value),
                    Some(other) => {
                        return Err(schema_error(format!(
                            "functions[].singleton must be a bool or null, got {other}"
                        )))
                    }
                },
            })
        })
        .collect()
}

fn parse_classes(value: Option<&Json>) -> Result<BTreeMap<String, ClassInfo>, VpcfError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let classes = value
        .as_object()
        .ok_or_else(|| schema_error(format!("classes must be an object, got {value}")))?;
    let mut out = BTreeMap::new();
    for (name, info) in classes {
        let info = info
            .as_object()
            .ok_or_else(|| schema_error(format!("class {name:?} is not an object: {info}")))?;
        let bases = match info.get("bases") {
            None | Some(Json::Null) => Vec::new(),
            Some(value) => strings(value, &format!("class {name:?} bases"))?,
        };
        let metadata = match info.get("metadata") {
            None | Some(Json::Null) => Vec::new(),
            Some(value) => strings(value, &format!("class {name:?} metadata"))?,
        };
        let fields = match info.get("fields") {
            None | Some(Json::Null) => Vec::new(),
            Some(value) => {
                let rows = value.as_array().ok_or_else(|| {
                    schema_error(format!("class {name:?} fields must be an array, got {value}"))
                })?;
                rows.iter()
                    .map(|row| {
                        let row = row.as_object().ok_or_else(|| {
                            schema_error(format!(
                                "a field of class {name:?} is not an object: {row}"
                            ))
                        })?;
                        Ok(ClassField {
                            name: string(row, "name")?,
                            r#type: optional_string(row.get("type"))?,
                            declared_in: optional_string(row.get("declaredIn"))?.unwrap_or_default(),
                        })
                    })
                    .collect::<Result<Vec<_>, VpcfError>>()?
            }
        };
        out.insert(
            name.clone(),
            ClassInfo {
                bases,
                fields,
                metadata,
            },
        );
    }
    Ok(out)
}

fn strings(value: &Json, what: &str) -> Result<Vec<String>, VpcfError> {
    let items = value
        .as_array()
        .ok_or_else(|| schema_error(format!("{what} must be an array, got {value}")))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| schema_error(format!("{what} entries must be strings, got {item}")))
        })
        .collect()
}

fn optional_string(value: Option<&Json>) -> Result<Option<String>, VpcfError> {
    match value {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(schema_error(format!(
            "expected a string or null, got {other}"
        ))),
    }
}

fn string(map: &Map<String, Json>, key: &str) -> Result<String, VpcfError> {
    match map.get(key) {
        Some(Json::String(text)) if !text.is_empty() => Ok(text.clone()),
        Some(other) => Err(schema_error(format!(
            "{key} must be a non-empty string, got {other}"
        ))),
        None => Err(schema_error(format!("{key} is required"))),
    }
}

fn integer(map: &Map<String, Json>, key: &str) -> Result<usize, VpcfError> {
    match map.get(key) {
        Some(Json::Number(number)) => number.as_u64().map(|value| value as usize).ok_or_else(|| {
            schema_error(format!("{key} must be a non-negative integer, got {number}"))
        }),
        Some(other) => Err(schema_error(format!(
            "{key} must be a non-negative integer, got {other}"
        ))),
        None => Err(schema_error(format!("{key} is required"))),
    }
}
