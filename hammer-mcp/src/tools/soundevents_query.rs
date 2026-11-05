//! `list_soundevents` (WP-S1): the read-only view over [`crate::soundevents::SoundEventIndex`] —
//! every soundevent name the game VPKs and the active addon carry, with the two routes the plan
//! fixes (`target/orch/plan-snd.md` §1):
//!
//! * the **list** route filters/pages the index: `query` (case-insensitive substring, or a glob
//!   when it carries `*`/`?`), `prefix` (case-insensitive name prefix), `source`
//!   (`any`/`addon`/`game`), `type` (exact `type` value), `file` (case-insensitive substring of
//!   the holding file), `hasPosition`, `limit` (1..=1000, default 100) and `offset`. The result
//!   carries `total`/`returned`/`offset`/`truncated`, one compact record per event, and a
//!   `summary` over **every** match (`bySource` counts the `sources` labels — an event in both
//!   the addon and a pak counts once in each — and `byType` the top 20 `type` values, an event
//!   without a `type` under `"(none)"`);
//! * the **detail** route (`name`, exact and case-sensitive: event names are hashed by the engine,
//!   so `Weapon_AK47.Single` and `weapon_ak47.single` are different events) returns the one
//!   event with its full `properties`, the base-first `resolved` chain and merged properties, the
//!   `children`/`vsnds` of the event with their `known` verdicts, and ignores the list filters.
//!   `name` together with `query` is refused (`InvalidArguments`: one is the list, the other the
//!   detail); an unknown `name` is `SoundEventNotFound` **with suggestions**.
//!
//! Both routes read the cached index through `soundevents::index_for` (never the bridge: this is
//! pure filesystem). `index.rebuilt` says whether this call built it. A missing game VPK is
//! `VpkMissing` naming the file — never an empty list that reads as "no soundevents"; an absent
//! addon directory, a corrupt `.vsndevts_c` and an unparseable `.vsndevts` are `notes` (the index
//! keeps answering from the sources that did read). Tests: `tests/soundevents_query.rs`.
use crate::soundevents::{SoundEventEntry, SoundEventIndex};
use crate::state::HammerState;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// `limit` default and bound (plan §1).
const LIMIT_DEFAULT: u64 = 100;
const LIMIT_MAX: u64 = 1000;
/// `byType` keeps this many values (the plan's "top 20").
const BY_TYPE_MAX: usize = 20;
/// How many suggestions a `SoundEventNotFound` carries.
const SUGGESTION_LIMIT: usize = 5;
/// The four sources a `SoundEventNotFound` reports having checked (the plan's data shape).
const SOURCES_CHECKED: [&str; 4] = ["addon-game", "addon-source", "vpk:csgo", "vpk:core"];

/// Accepted argument names (the schema says `additionalProperties:false` for the client; the
/// handler is the strict validator).
const KNOWN_ARGS: [&str; 10] = [
    "query",
    "prefix",
    "source",
    "type",
    "file",
    "hasPosition",
    "name",
    "includeProperties",
    "limit",
    "offset",
];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "list_soundevents",
            "List soundevents available to the game and the active addon (game VPKs + addon files; \
             no bridge). Use an entry's `name` verbatim as a sound event keyvalue. Pass `name` for \
             one event's full properties and base chain; the other filters apply to the list only.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "case-insensitive substring of the name; '*'/'?' make it a glob"
                },
                "prefix": {
                    "type": "string",
                    "description": "name prefix, case-insensitive (e.g. 'mcp.')"
                },
                "source": {
                    "type": "string",
                    "enum": ["any", "addon", "game"],
                    "description": "default any"
                },
                "type": {
                    "type": "string",
                    "description": "exact event `type` (e.g. 'csgo_mega')"
                },
                "file": {
                    "type": "string",
                    "description": "substring of the holding file path"
                },
                "hasPosition": {
                    "type": "boolean",
                    "description": "only events with a `position`"
                },
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "description": "exact name (case-sensitive): detail route for ONE event; ignores list filters"
                },
                "includeProperties": {
                    "type": "boolean",
                    "description": "include raw `properties` per listed event"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "default": LIMIT_DEFAULT
                },
                "offset": {
                    "type": "integer", "minimum": 0, "default": 0
                }
            },
            "additionalProperties": false
        })),
        list_soundevents,
    );
}

// -------------------------------------------------------------------------------------------
// arguments
// -------------------------------------------------------------------------------------------

/// An unknown argument: `InvalidArguments` before any work happens.
fn check_known_args(args: &Value) -> Result<(), ToolError> {
    let Some(map) = args.as_object() else {
        return Err(ToolError::invalid_arguments(
            "arguments must be an object".to_string(),
        ));
    };
    for name in map.keys() {
        if !KNOWN_ARGS.contains(&name.as_str()) {
            return Err(ToolError::invalid_arguments(format!(
                "unknown argument {name:?} (accepted: {})",
                KNOWN_ARGS.join(", ")
            )));
        }
    }
    Ok(())
}

/// A non-empty string argument; `None` when absent, `InvalidArguments` for anything else
/// (including an empty string: an empty filter matches everything while pretending to filter).
fn text_arg<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.as_str())),
        Some(Value::String(_)) => Err(ToolError::invalid_arguments(format!(
            "{name} is empty: omit it instead of filtering on nothing"
        ))),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a string, got {other}"
        ))),
    }
}

/// A boolean flag argument with a default.
fn bool_arg(args: &Value, name: &str, default: bool) -> Result<bool, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "{name} must be a boolean, got {other}"
        ))),
    }
}

/// `limit?` in 1..=[`LIMIT_MAX`], default [`LIMIT_DEFAULT`].
fn limit_arg(args: &Value) -> Result<usize, ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT as usize),
        Some(Value::Number(number)) => match number.as_u64() {
            Some(limit) if (1..=LIMIT_MAX).contains(&limit) => Ok(limit as usize),
            _ => Err(ToolError::invalid_arguments(format!(
                "limit must be an integer in 1..={LIMIT_MAX}, got {number}"
            ))),
        },
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "limit must be an integer in 1..={LIMIT_MAX}, got {other}"
        ))),
    }
}

/// `offset?` >= 0 (default 0).
fn offset_arg(args: &Value) -> Result<usize, ToolError> {
    match args.get("offset") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(number)) => number.as_u64().map(|value| value as usize).ok_or_else(
            || {
                ToolError::invalid_arguments(format!(
                    "offset must be a non-negative integer, got {number}"
                ))
            },
        ),
        Some(other) => Err(ToolError::invalid_arguments(format!(
            "offset must be a non-negative integer, got {other}"
        ))),
    }
}

/// Which `sources` an entry must carry for the `source` filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceFilter {
    Any,
    Addon,
    Game,
}

impl SourceFilter {
    fn parse(args: &Value) -> Result<SourceFilter, ToolError> {
        match args.get("source") {
            None | Some(Value::Null) => Ok(SourceFilter::Any),
            Some(Value::String(text)) => match text.as_str() {
                "any" => Ok(SourceFilter::Any),
                "addon" => Ok(SourceFilter::Addon),
                "game" => Ok(SourceFilter::Game),
                other => Err(ToolError::invalid_arguments(format!(
                    "source must be \"any\", \"addon\" or \"game\", got {other:?}"
                ))),
            },
            Some(other) => Err(ToolError::invalid_arguments(format!(
                "source must be \"any\", \"addon\" or \"game\", got {other}"
            ))),
        }
    }

    fn keeps(self, entry: &SoundEventEntry) -> bool {
        match self {
            SourceFilter::Any => true,
            SourceFilter::Addon => entry.sources.iter().any(|source| source.is_addon()),
            SourceFilter::Game => entry.sources.iter().any(|source| !source.is_addon()),
        }
    }
}

/// The list route's filters, validated once.
struct Filters {
    /// `(lowercased pattern, is_glob)`.
    query: Option<(String, bool)>,
    prefix: Option<String>,
    source: SourceFilter,
    type_: Option<String>,
    file: Option<String>,
    has_position: bool,
}

impl Filters {
    fn parse(args: &Value) -> Result<Filters, ToolError> {
        Ok(Filters {
            query: text_arg(args, "query")?.map(|query| {
                let glob = query.contains('*') || query.contains('?');
                (query.to_lowercase(), glob)
            }),
            prefix: text_arg(args, "prefix")?.map(str::to_lowercase),
            source: SourceFilter::parse(args)?,
            type_: text_arg(args, "type")?.map(str::to_string),
            file: text_arg(args, "file")?.map(str::to_lowercase),
            has_position: bool_arg(args, "hasPosition", false)?,
        })
    }

    fn keeps(&self, entry: &SoundEventEntry) -> bool {
        if !self.source.keeps(entry) {
            return false;
        }
        if self.has_position && entry.position.is_none() {
            return false;
        }
        if let Some(prefix) = &self.prefix {
            if !entry.name.to_lowercase().starts_with(prefix.as_str()) {
                return false;
            }
        }
        if let Some(type_) = &self.type_ {
            if entry.type_.as_deref() != Some(type_.as_str()) {
                return false;
            }
        }
        if let Some(file) = &self.file {
            if !entry.file.to_lowercase().contains(file.as_str()) {
                return false;
            }
        }
        match &self.query {
            None => true,
            Some((pattern, glob)) => {
                let lowered = entry.name.to_lowercase();
                if *glob {
                    glob_match(pattern, &lowered)
                } else {
                    lowered.contains(pattern.as_str())
                }
            }
        }
    }
}

/// A bounded glob (`*` any run, `?` one char, everything else literal) as a DP over the pattern
/// and the text — never a backtracking blow-up on a pathological pattern (the model catalog's
/// `glob_match`, verbatim in behaviour).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
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

// -------------------------------------------------------------------------------------------
// records
// -------------------------------------------------------------------------------------------

/// One event's compact record (both routes share it; the detail route adds `properties`).
fn entry_json(entry: &SoundEventEntry) -> Value {
    json!({
        "name": entry.name,
        "type": entry.type_,
        "base": entry.base,
        "volume": entry.volume,
        "source": entry.source().as_str(),
        "sources": entry.sources.iter().map(|source| source.as_str()).collect::<Vec<_>>(),
        "compiled": entry.compiled,
        "file": entry.file,
        "vsndCount": entry.vsnds.len(),
        "childCount": entry.children.len(),
        "position": entry.position.map(|position| json!([position[0], position[1], position[2]])),
    })
}

/// The `index` block both routes carry.
fn index_json(index: &SoundEventIndex, rebuilt: bool) -> Value {
    json!({
        "files": index.files(),
        "events": index.len(),
        "builtMs": index.built_ms(),
        "rebuilt": rebuilt,
    })
}

/// The `SoundEventNotFound` for an unknown `name` (suggestions attached).
fn not_found(index: &SoundEventIndex, name: &str) -> ToolError {
    ToolError::new(
        "SoundEventNotFound",
        "SoundEventNotFound",
        format!(
            "no soundevent named {name:?} in the {} file(s) the index read ({} events)",
            index.files(),
            index.len()
        ),
    )
    .with_data(json!({
        "name": name,
        "sourcesChecked": SOURCES_CHECKED,
    }))
    .did_you_mean(index.suggestions(name, SUGGESTION_LIMIT))
    .retry_safe(true)
}

/// `summary.bySource`/`byType` over every match (not just the page).
fn summary(matches: &[&SoundEventEntry]) -> Value {
    let mut by_source: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut by_type: BTreeMap<String, u64> = BTreeMap::new();
    for entry in matches {
        for source in &entry.sources {
            *by_source.entry(source.as_str()).or_default() += 1;
        }
        let type_ = entry
            .type_
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        *by_type.entry(type_).or_default() += 1;
    }
    let mut types: Vec<(String, u64)> = by_type.into_iter().collect();
    // the most common first, alphabetical on a tie (deterministic), then the top 20
    types.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    types.truncate(BY_TYPE_MAX);
    let by_type: Map<String, Value> = types
        .into_iter()
        .map(|(name, count)| (name, Value::from(count)))
        .collect();
    json!({
        "bySource": by_source,
        "byType": by_type,
    })
}

// -------------------------------------------------------------------------------------------
// list_soundevents
// -------------------------------------------------------------------------------------------

pub async fn list_soundevents(
    state: HammerState,
    _ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    check_known_args(&args)?;
    let name = text_arg(&args, "name")?;
    let query = text_arg(&args, "query")?;
    if name.is_some() && query.is_some() {
        return Err(ToolError::invalid_arguments(
            "name is the detail route and ignores the list filters: pass either name or query, \
             not both"
                .to_string(),
        ));
    }
    let (index, rebuilt) = crate::soundevents::index_for_report(&state).await?;
    match name {
        Some(name) => detail(&index, name, rebuilt),
        None => list(&args, &index, rebuilt),
    }
}

/// The detail route: the one event plus its resolved chain and the children/vsnd verdicts.
fn detail(index: &SoundEventIndex, name: &str, rebuilt: bool) -> Result<ToolOutput, ToolError> {
    let Some(entry) = index.get(name) else {
        return Err(not_found(index, name));
    };
    let resolved = index.resolve_chain(name).ok_or_else(|| {
        ToolError::internal(format!(
            "{name} is in the soundevent index but resolves to nothing (index bug)"
        ))
    })?;
    let mut event = entry_json(entry);
    event
        .as_object_mut()
        .expect("entry_json builds an object")
        .insert("properties".into(), entry.properties.clone());
    let children: Vec<Value> = entry
        .children
        .iter()
        .map(|child| {
            json!({
                "name": child,
                "known": index.get(child).is_some(),
            })
        })
        .collect();
    let vsnds: Vec<Value> = entry
        .vsnds
        .iter()
        .map(|path| {
            json!({
                "path": path,
                "known": index.sound_asset_known(path),
            })
        })
        .collect();
    Ok(json!({
        "event": event,
        "resolved": {
            "chain": resolved.chain,
            "merged": resolved.merged,
            "cycle": resolved.cycle,
        },
        "children": children,
        "vsnds": vsnds,
        "index": index_json(index, rebuilt),
        "notes": index.notes(),
    })
    .into())
}

/// The list route: the filters, the page and the summary.
fn list(args: &Value, index: &SoundEventIndex, rebuilt: bool) -> Result<ToolOutput, ToolError> {
    let filters = Filters::parse(args)?;
    let limit = limit_arg(args)?;
    let offset = offset_arg(args)?;
    let include_properties = bool_arg(args, "includeProperties", false)?;

    let matches: Vec<&SoundEventEntry> = index
        .entries()
        .iter()
        .filter(|entry| filters.keeps(entry))
        .collect();
    let total = matches.len();
    let page: Vec<Value> = matches
        .iter()
        .skip(offset)
        .take(limit)
        .map(|entry| {
            let mut record = entry_json(entry);
            if include_properties {
                record
                    .as_object_mut()
                    .expect("entry_json builds an object")
                    .insert("properties".into(), entry.properties.clone());
            }
            record
        })
        .collect();
    let returned = page.len();
    Ok(json!({
        "total": total,
        "returned": returned,
        "offset": offset,
        "truncated": offset + returned < total,
        "events": page,
        "summary": summary(&matches),
        "index": index_json(index, rebuilt),
        "notes": index.notes(),
    })
    .into())
}
