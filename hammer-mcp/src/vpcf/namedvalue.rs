//! `m_NamedValueLocals` (bridge type 8): `ParticleNamedValueSource_t` entries in the form pet
//! writes them (read back live 2026-10-13, build 2000930: `target/orch/live-facts.md` "the answer",
//! `target/orch/live2/namedvalue.*`).
//!
//! Type 8 is one of the two structural arrays ([`crate::vpcf::ARRAYS`]): its entries are plain
//! structs, not registry-backed particle functions, and — like `m_Children` — pet writes no
//! `_class` (an `_class` equal to [`ELEMENT_CLASS`] is still accepted by
//! [`crate::vpcf::Particle::validate`]; it is simply not what pet's writer emits).
//!
//! ```text
//! m_NamedValueLocals =
//! [
//!     {
//!         m_Name = "mcp_float"
//!         m_DefaultConfig =
//!         {
//!             m_ConfigName = ""
//!             m_ConfigValue = 1.0
//!             m_BoundValuePath = ""
//!             m_iAttachType = "PATTACH_INVALID"
//!             m_strEntityScope = ""
//!             m_strAttachmentName = ""
//!         }
//!         m_ValueType = "PVAL_FLOAT"
//!     },
//! ]
//! ```
//!
//! That is pet's own serialization, byte for byte: `m_Name`, then `m_IsPublic` **only when it is
//! `false`** (`true` is the class default and pet omits default-valued keys), then
//! `m_DefaultConfig` in that field order, then `m_ValueType`. A private entry therefore reads
//! `m_Name`, `m_IsPublic = false`, `m_DefaultConfig`, `m_ValueType`. Two further live readings
//! shape this module:
//!
//! * an entry left without a `m_DefaultConfig` gets a full default config from pet, with
//!   `m_ConfigValue = null` — so [`CONFIG_VALUE_UNSET`] (KV3 `null`) is pet's own spelling for an
//!   unset config value (`namedvalue.noconfig.txt`);
//! * a non-string `m_ValueType` (e.g. the number `2`) is silently turned into `"PVAL_VOID"` by pet,
//!   the field's default (`namedvalue.numtype.txt`).
//!
//! The fields themselves come from the GameTracking schema dump (`hammer-mcp/research/pet-re.md`,
//! "m_NamedValueLocals"): element class `ParticleNamedValueSource_t` { `m_Name` CUtlString,
//! `m_IsPublic` bool (default true), `m_ValueType` CPulseValueFullType (default `"PVAL_VOID"`, so a
//! string such as `"PVAL_FLOAT"`), `m_DefaultConfig` `ParticleNamedValueConfiguration_t` }.
//! Everything the writer pins is a constant in this module: [`KEY`], [`ELEMENT_CLASS`],
//! [`CONFIG_CLASS`], [`VALUE_TYPES`], the field/key constants below, [`ATTACH_TYPE_INVALID`] and
//! [`CONFIG_VALUE_UNSET`] — plus `fixtures/vpcf/with_namedvalue.vpcf`, which holds pet's entry text
//! and which [`build_entry`] on the probe reproduces byte for byte.
//!
//! [`validate_entries`] checks what a snapshot *can* say about the entries: a non-empty, unique
//! `m_Name`, an `m_ValueType` that is a `PVAL_*` string (a `PVAL_*` the Pulse enum does not know is
//! a warning: the enum grows), and `m_DefaultConfig` keys that exist on [`CONFIG_CLASS`]. A
//! snapshot that lacks `ParticleNamedValueConfiguration_t` (the committed
//! `research/pet-schema.json` does: the file predates the class regen) cannot check the config at
//! all and warns, telling the caller how to refresh.

use crate::vpcf::{Issue, Schema};
use cs2_kv3::{Object, Value};
use serde_json::Value as Json;

/// The file key of bridge type 8 (`ARRAYS[8]`).
pub const KEY: &str = "m_NamedValueLocals";

/// The struct class a `m_NamedValueLocals` entry is an instance of: the `_class` an entry may
/// carry, and the class the field list comes from.
pub const ELEMENT_CLASS: &str = "ParticleNamedValueSource_t";

/// The class of an entry's `m_DefaultConfig`.
pub const CONFIG_CLASS: &str = "ParticleNamedValueConfiguration_t";

/// The `m_ValueType` strings the Pulse enum knows (`EPulseValueType` / `PVAL_*`). A value that
/// starts with `PVAL_` but is not in this list is a **warning**, never an error: the enum is
/// version-dependent and pet accepts strings the snapshot predates.
pub const VALUE_TYPES: &[&str] = &[
    "PVAL_BOOL",
    "PVAL_INT",
    "PVAL_FLOAT",
    "PVAL_VEC3",
    "PVAL_STRING",
    "PVAL_COLOR_RGB",
    "PVAL_TRANSFORM",
    "PVAL_EHANDLE",
    "PVAL_RESOURCE",
];

/// `m_Name`: the variable's name, unique inside the array.
pub const NAME_FIELD: &str = "m_Name";
/// `m_IsPublic` (bool, class default `true`). pet writes the key only when the value is `false`;
/// `true` is its default and is omitted from the file.
pub const IS_PUBLIC_FIELD: &str = "m_IsPublic";
/// `m_ValueType` (`CPulseValueFullType`, written as a string).
pub const VALUE_TYPE_FIELD: &str = "m_ValueType";
/// `m_DefaultConfig` (`ParticleNamedValueConfiguration_t`).
pub const DEFAULT_CONFIG_FIELD: &str = "m_DefaultConfig";

/// The `m_DefaultConfig` keys, in the file order pet writes them.
pub const CONFIG_FIELDS: [&str; 6] = [
    "m_ConfigName",
    "m_ConfigValue",
    "m_BoundValuePath",
    "m_iAttachType",
    "m_strEntityScope",
    "m_strAttachmentName",
];

/// pet's `m_iAttachType` string for an entry that is not attached (the field's default).
pub const ATTACH_TYPE_INVALID: &str = "PATTACH_INVALID";

/// What [`build_entry`] writes for `m_ConfigValue` when the caller passes no `default_value`:
/// KV3 `null`, pet's own "unset" spelling of the field (confirmed live: pet fills a missing config
/// with `m_ConfigValue = null`, `target/orch/live2/namedvalue.noconfig.txt`).
pub const CONFIG_VALUE_UNSET: Json = Json::Null;

/// The empty string pet writes for the config's string fields that are unset.
const CONFIG_STRING_UNSET: &str = "";

/// The entry for [`KEY`] in pet's key order: no `_class`, then `m_Name`, `m_IsPublic` **only when
/// `is_public` is `false`** (pet omits the field when it is `true`, its default), then
/// `m_DefaultConfig` — carrying [`CONFIG_FIELDS`] in order, `m_ConfigValue` set to `default_value`
/// (or [`CONFIG_VALUE_UNSET`]) — and last `m_ValueType`.
///
/// `default_value` is the `m_ConfigValue` a caller supplies (a JSON value, converted with
/// [`cs2_kv3::from_json`]); a JSON value with no KV3 spelling (`{"$blob":"zz"}`) becomes KV3 `null`
/// here — `add_variable` rejects those before calling this, so nothing can silently lose a value
/// through the op.
pub fn build_entry(
    name: &str,
    value_type: &str,
    is_public: bool,
    default_value: Option<Json>,
) -> Object {
    let mut config = Object::new();
    for key in CONFIG_FIELDS {
        let value = match key {
            // The only key the caller can steer: `default_value` (JSON, [`CONFIG_VALUE_UNSET`]
            // when it is `None`) converted to KV3. A JSON value with no KV3 spelling becomes
            // KV3 `null` — `add_variable` rejects those before calling this.
            "m_ConfigValue" => {
                cs2_kv3::from_json(default_value.as_ref().unwrap_or(&CONFIG_VALUE_UNSET))
                    .unwrap_or(Value::Null)
            }
            "m_iAttachType" => Value::String(ATTACH_TYPE_INVALID.to_string()),
            _ => Value::String(CONFIG_STRING_UNSET.to_string()),
        };
        config.insert(key.to_string(), value);
    }

    let mut entry = Object::new();
    entry.insert(NAME_FIELD.to_string(), Value::String(name.to_string()));
    // pet's writer drops `m_IsPublic = true`: it is the field's default.
    if !is_public {
        entry.insert(IS_PUBLIC_FIELD.to_string(), Value::Bool(false));
    }
    entry.insert(DEFAULT_CONFIG_FIELD.to_string(), Value::Object(config));
    entry.insert(
        VALUE_TYPE_FIELD.to_string(),
        Value::String(value_type.to_string()),
    );
    entry
}

/// The problems of the `m_NamedValueLocals` entries of `root`: one [`Issue`] per entry problem, at
/// the entry's path (`"m_NamedValueLocals/1"`), so the array loop of
/// [`crate::vpcf::Particle::validate`] keeps its shape.
///
/// * `m_Name`: a non-empty string, and unique across the array (a duplicate is an error on the
///   later entry, naming the first one);
/// * `m_ValueType`: **must be a string** (error otherwise: pet silently rewrites a non-string with
///   `"PVAL_VOID"`, so no live pet output can carry one), starting `PVAL_` (error otherwise; a
///   `PVAL_*` outside [`VALUE_TYPES`] is a warning);
/// * `m_DefaultConfig`: when present, an object whose keys exist on [`CONFIG_CLASS`] (error).
///   Missing is **not** a problem: pet supplies a full default config. `None` when `schema` is
///   `None` **or** lacks [`CONFIG_CLASS`]: the keys cannot be checked, so an entry that carries a
///   `m_DefaultConfig` gets a warning naming the fix (`particle_types {writeSnapshot:true}`). An
///   entry without a `m_DefaultConfig` has nothing to check and stays silent.
/// * `m_IsPublic`: not checked — `true` (pet's default, which pet drops again on save) and `false`
///   are both fine.
///
/// A missing/non-string `_class` is not this function's business: [`crate::vpcf`]'s array loop
/// covers it (an absent `_class` or one equal to [`ELEMENT_CLASS`] is fine).
pub fn validate_entries(root: &Object, schema: Option<&Schema>) -> Vec<Issue> {
    let Some(items) = root.get(KEY).and_then(Value::as_array) else {
        return Vec::new();
    };
    let config = schema.and_then(|schema| schema.class(CONFIG_CLASS));
    let mut issues = Vec::new();
    let mut names: Vec<(String, usize)> = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let path = format!("{KEY}/{index}");
        let Some(entry) = item.as_object() else {
            // A non-object entry is already a structural error.
            continue;
        };

        match entry.get(NAME_FIELD) {
            Some(Value::String(name)) if name.is_empty() => issues.push(Issue::error(
                &path,
                format!("{NAME_FIELD} is empty (it names the variable)"),
            )),
            Some(Value::String(name)) => match names.iter().find(|(seen, _)| seen == name) {
                Some((_, first)) => issues.push(Issue::error(
                    &path,
                    format!(
                        "duplicate {NAME_FIELD} {name:?} (already used by {KEY}/{first}): \
                         names must be unique"
                    ),
                )),
                None => names.push((name.clone(), index)),
            },
            Some(other) => issues.push(Issue::error(
                &path,
                format!(
                    "{NAME_FIELD} is not a string (the field is a CUtlString), got {}",
                    cs2_kv3::to_json(other)
                ),
            )),
            None => issues.push(Issue::error(
                &path,
                format!("{NAME_FIELD} is missing (a {ELEMENT_CLASS} needs one)"),
            )),
        }

        match entry.get(VALUE_TYPE_FIELD) {
            Some(Value::String(value_type)) if value_type.starts_with("PVAL_") => {
                if !VALUE_TYPES.contains(&value_type.as_str()) {
                    issues.push(Issue::warning(
                        &path,
                        format!(
                            "{VALUE_TYPE_FIELD} {value_type:?} is not one of the known Pulse \
                             value types ({})",
                            VALUE_TYPES.join(", ")
                        ),
                    ));
                }
            }
            Some(Value::String(value_type)) => issues.push(Issue::error(
                &path,
                format!(
                    "{VALUE_TYPE_FIELD} {value_type:?} does not start with \"PVAL_\" (the field \
                     is a CPulseValueFullType written as a string)"
                ),
            )),
            Some(other) => issues.push(Issue::error(
                &path,
                format!(
                    "{VALUE_TYPE_FIELD} is not a string (the field is a CPulseValueFullType), \
                     got {}",
                    cs2_kv3::to_json(other)
                ),
            )),
            None => issues.push(Issue::error(
                &path,
                format!("{VALUE_TYPE_FIELD} is missing (a {ELEMENT_CLASS} needs one)"),
            )),
        }

        match entry.get(DEFAULT_CONFIG_FIELD) {
            None => {}
            Some(Value::Object(config_entry)) => match config {
                Some(info) => {
                    for key in config_entry.keys() {
                        if !info.has_field(key) {
                            issues.push(Issue::error(
                                &path,
                                format!(
                                    "unknown {DEFAULT_CONFIG_FIELD} key {key:?} on class \
                                     {CONFIG_CLASS}"
                                ),
                            ));
                        }
                    }
                }
                None => issues.push(Issue::warning(
                    &path,
                    format!(
                        "snapshot has no {CONFIG_CLASS}: refresh with particle_types \
                         {{writeSnapshot:true}}"
                    ),
                )),
            },
            Some(other) => issues.push(Issue::error(
                &path,
                format!(
                    "{DEFAULT_CONFIG_FIELD} is not an object (it is a {CONFIG_CLASS}), got {}",
                    cs2_kv3::to_json(other)
                ),
            )),
        }
    }

    issues
}
