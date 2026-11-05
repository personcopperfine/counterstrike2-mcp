//! `get_scene_info {query?, fields?, limit?, source?, path?}` (WP2.3): one compact read that
//! answers "what is in this map" before the `list_*` family: counts per class and kind, bounds,
//! spawns, and the first `limit` entities whose classname or targetname contains `query`, with
//! `fields` choosing the columns.
//!
//! Same two routes and the same default as `get_entity` / `find_entities`
//! ([`super::entities_read`]): live = the Hammer document (`entity.find` up to 2000 entities +
//! `node.list` for the mesh count when the bridge has it), saved = the `.vmap` (with `staleRead`).
//! The `list_entities` / `list_meshes` / `list_props` / `list_triggers` tools stay: they carry
//! per-item detail this overview deliberately leaves out.
use super::entities_read::{
    need_live, path_schema, pick_source, refuse_foreign, source_schema, stamp, Source,
};
use crate::state::HammerState;
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::{CallCtx, Mutability::*, Registry, ToolError, ToolOutput, ToolSpec};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const LIMIT_DEFAULT: usize = 25;
const LIMIT_MAX: usize = 500;
const SPAWN_CAP: usize = 40;
/// Classes listed in `classes` (biggest first); the rest are counted in `otherClasses`.
const CLASS_CAP: usize = 40;
/// `node.list` page for the live mesh count: ~100 B a node keeps the reply under the bridge's
/// 512 KB result cap (a bigger page is answered "response too large").
const LIVE_MESH_PAGE: u64 = 4000;
/// `entity.find`'s own maximum: past it the live counts are partial and say so.
const LIVE_ENTITY_CAP: u64 = 2000;
const FIELDS: [&str; 4] = ["nodeID", "classname", "targetname", "origin"];

pub fn register(r: &mut Registry<HammerState>) {
    r.register(
        ToolSpec::new(
            "get_scene_info",
            "Overview of the map: entity counts per class, mesh count, bounds, spawns, and the first `limit` entities whose classname/targetname contains `query`. Start here; find_entities/get_entity/list_meshes for detail. source live = Hammer document, saved = .vmap on disk.",
            ReadOnly,
            &[],
        )
        .with_schema(json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string", "minLength": 1,
                    "description": "case-insensitive substring of classname/targetname"
                },
                "fields": {
                    "type": "array", "items": {"type": "string", "enum": FIELDS},
                    "description": "columns"
                },
                "limit": {
                    "type": "integer", "minimum": 1, "maximum": LIMIT_MAX,
                    "description": "max items"
                },
                "source": source_schema(),
                "path": path_schema()
            },
            "additionalProperties": false
        })),
        get_scene_info,
    );
}

/// One entity row, whichever route produced it.
struct Row {
    node_id: Option<i64>,
    classname: String,
    targetname: String,
    origin: Option<[f64; 3]>,
}

/// One mesh: its bounds when known (saved), else just its origin (live).
struct MeshBox {
    min: [f64; 3],
    max: [f64; 3],
}

fn parse_args(args: &Value) -> Result<(Option<String>, Vec<&'static str>, usize), ToolError> {
    let query = match args.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(q)) if !q.is_empty() => Some(q.to_lowercase()),
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "query must be a non-empty string, got {other}"
            )))
        }
    };
    let fields = match args.get("fields") {
        None | Some(Value::Null) => FIELDS.to_vec(),
        Some(Value::Array(list)) if !list.is_empty() => {
            let mut out = Vec::new();
            for f in list {
                let name = f.as_str().and_then(|n| FIELDS.iter().copied().find(|k| *k == n));
                match name {
                    Some(n) => {
                        if !out.contains(&n) {
                            out.push(n)
                        }
                    }
                    None => {
                        return Err(ToolError::invalid_arguments(format!(
                            "fields: {f} is not one of {FIELDS:?}"
                        )))
                    }
                }
            }
            out
        }
        Some(other) => {
            return Err(ToolError::invalid_arguments(format!(
                "fields must be a non-empty array of {FIELDS:?}, got {other}"
            )))
        }
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => LIMIT_DEFAULT,
        Some(v) => match v.as_u64() {
            Some(n) if (1..=LIMIT_MAX as u64).contains(&n) => n as usize,
            _ => {
                return Err(ToolError::invalid_arguments(format!(
                    "limit must be an integer in 1..={LIMIT_MAX}, got {v}"
                )))
            }
        },
    };
    Ok((query, fields, limit))
}

fn is_spawn(class: &str) -> bool {
    class.starts_with("info_player_") || class == "info_deathmatch_spawn"
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn vec3(v: [f64; 3]) -> Value {
    json!([round1(v[0]), round1(v[1]), round1(v[2])])
}

/// The overview from rows + meshes (pure: both routes and the tests use it).
fn build(
    rows: &[Row],
    mesh_count: Option<usize>,
    meshes: &[MeshBox],
    query: Option<&str>,
    fields: &[&str],
    limit: usize,
) -> Value {
    let mut classes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    let mut any = false;
    // the world geometry alone (point entities the template leaves far away do not count)
    let mut mesh_min = [f64::INFINITY; 3];
    let mut mesh_max = [f64::NEG_INFINITY; 3];
    let mut grow = |lo: [f64; 3], hi: [f64; 3]| {
        any = true;
        for i in 0..3 {
            min[i] = min[i].min(lo[i]);
            max[i] = max[i].max(hi[i]);
        }
    };
    let mut spawns = Vec::new();
    let mut spawn_total = 0usize;
    let mut items = Vec::new();
    let mut matched = 0usize;
    for row in rows {
        *classes.entry(row.classname.as_str()).or_default() += 1;
        if let Some(o) = row.origin {
            grow(o, o);
        }
        if is_spawn(&row.classname) {
            spawn_total += 1;
            if spawns.len() < SPAWN_CAP {
                spawns.push(json!({
                    "nodeID": row.node_id,
                    "classname": row.classname,
                    "origin": row.origin.map(vec3),
                }));
            }
        }
        let hit = query.is_none_or(|q| {
            row.classname.to_lowercase().contains(q) || row.targetname.to_lowercase().contains(q)
        });
        if hit {
            matched += 1;
            if items.len() < limit {
                let mut item = Map::new();
                for f in fields {
                    let v = match *f {
                        "nodeID" => json!(row.node_id),
                        "classname" => json!(row.classname),
                        "targetname" => json!(row.targetname),
                        _ => row.origin.map(vec3).unwrap_or(Value::Null),
                    };
                    item.insert((*f).into(), v);
                }
                items.push(Value::Object(item));
            }
        }
    }
    for mesh in meshes {
        grow(mesh.min, mesh.max);
        for i in 0..3 {
            mesh_min[i] = mesh_min[i].min(mesh.min[i]);
            mesh_max[i] = mesh_max[i].max(mesh.max[i]);
        }
    }
    let mut by_count: Vec<(&str, u64)> = classes.iter().map(|(k, v)| (*k, *v)).collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    // an ARRAY, biggest first: a JSON object would come out alphabetical (no preserve_order)
    let class_list: Vec<Value> =
        by_count.iter().take(CLASS_CAP).map(|(k, v)| json!([k, v])).collect();
    let other_classes = by_count.len().saturating_sub(CLASS_CAP);
    json!({
        "counts": {
            "entities": rows.len(),
            "classes": classes.len(),
            "meshes": mesh_count,
        },
        "classes": class_list,
        "otherClasses": other_classes,
        "bounds": any.then(|| json!({"min": vec3(min), "max": vec3(max)})),
        "meshBounds": (!meshes.is_empty()).then(|| json!({"min": vec3(mesh_min), "max": vec3(mesh_max)})),
        "spawns": spawns,
        "spawnsTruncated": spawn_total > spawns.len(),
        "query": query,
        "matched": matched,
        "items": items,
        "truncated": matched > items.len(),
    })
}

pub async fn get_scene_info(
    state: HammerState,
    ctx: CallCtx,
    args: Value,
) -> Result<ToolOutput, ToolError> {
    let (query, fields, limit) = parse_args(&args)?;
    let picked = pick_source(&state, &ctx, &args, &["path"], &["entity.find"]).await?;
    let mut out = match picked.source {
        Source::Saved => {
            let (host, document, is_active, dirty) =
                super::entities_file::resolve_document(&state, &ctx, &args).await?;
            let rows: Vec<Row> = document
                .entities()
                .iter()
                .map(|e| Row {
                    node_id: e.node_id,
                    classname: e.classname.clone(),
                    targetname: e.targetname.clone().unwrap_or_default(),
                    origin: e.origin,
                })
                .collect();
            // a mesh with no vertices still counts, but bounds to [0,0,0]: keep it out of the box
            let boxes: Vec<MeshBox> = document
                .meshes()
                .iter()
                .filter(|m| !m.vertices.is_empty())
                .map(|m| {
                    let (min, max) = m.bounds();
                    MeshBox { min, max }
                })
                .collect();
            let mut body = build(
                &rows,
                Some(document.meshes().len()),
                &boxes,
                query.as_deref(),
                &fields,
                limit,
            );
            if !document.prefabs().is_empty() {
                let body = body.as_object_mut().expect("object");
                body["counts"]["prefabs"] = json!(document.prefabs().len());
                body.insert(
                    "prefabs".into(),
                    Value::Array(document.prefabs().iter().take(SPAWN_CAP).map(|p| p.to_json()).collect()),
                );
            }
            let (stale, reason) = super::entities_file::stale_read(&state, is_active, dirty);
            let mut env = super::entities_file::read_envelope(
                &state,
                Value::Null,
                stale,
                reason,
            );
            // `source` is the request argument's name: the file goes under `file` here
            env.remove("source");
            let file = super::entities_file::source_json(&host, &document);
            env.insert(
                "file".into(),
                json!({"hostPath": file["hostPath"], "mtime": file["mtime"]}),
            );
            env.append(body.as_object_mut().expect("object"));
            ToolOutput::from(Value::Object(env))
        }
        Source::Live => {
            refuse_foreign(&args, Source::Live, &["path"], Source::Saved)?;
            need_live(&state, &ctx, &["entity.find"]).await?;
            let deadline = ctx.budget.deadline_ms()?;
            let (rows, entities_truncated, meshes, mesh_count, mesh_note) = {
                let mut bridge = state.bridge.lock().await;
                let reply = bridge
                    .request(
                        "entity.find",
                        json!({"limit": LIVE_ENTITY_CAP}),
                        RequestOpts::deadline(deadline),
                    )
                    .await
                    .map_err(ToolError::from)?;
                let rows: Vec<Row> = reply
                    .result
                    .get("entities")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .map(|e| Row {
                                node_id: e.get("nodeID").and_then(Value::as_i64),
                                classname: e
                                    .get("classname")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                targetname: e
                                    .get("targetname")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                origin: e
                                    .get("origin")
                                    .and_then(Value::as_array)
                                    .filter(|a| a.len() == 3)
                                    .map(|a| {
                                        [
                                            a[0].as_f64().unwrap_or(0.0),
                                            a[1].as_f64().unwrap_or(0.0),
                                            a[2].as_f64().unwrap_or(0.0),
                                        ]
                                    }),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let truncated = reply.result.get("truncated").and_then(Value::as_bool) == Some(true);
                let has_node_list = bridge.missing(&["node.list"]).is_empty();
                let mut meshes = Vec::new();
                // None = the mesh count is unknown (no node.list, a failed or truncated page)
                let mut mesh_count = None;
                let mut mesh_note = None;
                if has_node_list {
                    match bridge
                        .request(
                            "node.list",
                            json!({"class": "mesh", "limit": LIVE_MESH_PAGE}),
                            RequestOpts::deadline(deadline),
                        )
                        .await
                    {
                        Ok(reply) => {
                            for n in reply
                                .result
                                .get("nodes")
                                .and_then(Value::as_array)
                                .map(Vec::as_slice)
                                .unwrap_or(&[])
                            {
                                let o = n.get("origin").and_then(Value::as_array);
                                if let Some(o) = o.filter(|a| a.len() == 3) {
                                    let p = [
                                        o[0].as_f64().unwrap_or(0.0),
                                        o[1].as_f64().unwrap_or(0.0),
                                        o[2].as_f64().unwrap_or(0.0),
                                    ];
                                    meshes.push(MeshBox { min: p, max: p });
                                }
                            }
                            if reply.result.get("truncated").and_then(Value::as_bool) == Some(true) {
                                mesh_note = Some(format!(
                                    "counts.meshes is null: more than {LIVE_MESH_PAGE} meshes \
                                     (node.list truncated); {} were seen, use list_meshes"
                                    , meshes.len()
                                ));
                            } else {
                                mesh_count = Some(meshes.len());
                            }
                        }
                        Err(error) => {
                            mesh_note = Some(format!(
                                "counts.meshes is null: node.list failed ({})",
                                error.message
                            ));
                        }
                    }
                }
                (rows, truncated, meshes, mesh_count, mesh_note)
            };
            let mut body = build(&rows, mesh_count, &meshes, query.as_deref(), &fields, limit);
            let m = body.as_object_mut().expect("object");
            let mut notes = vec![
                "live bounds use entity and mesh ORIGINS (mesh extents need list_meshes on the \
                 saved file)"
                    .to_string(),
            ];
            notes.extend(mesh_note);
            if entities_truncated {
                notes.push(
                    "the live document has more than 2000 entities: counts are partial".into(),
                );
            }
            m.insert("note".into(), json!(notes.join("; ")));
            ToolOutput::from(body)
        }
    };
    out = stamp(out, &picked);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, class: &str, name: &str, origin: [f64; 3]) -> Row {
        Row { node_id: Some(id), classname: class.into(), targetname: name.into(), origin: Some(origin) }
    }

    #[test]
    fn mesh_bounds_ignore_a_far_point_entity() {
        let rows = vec![row(1, "cs_minimap_boundary", "", [50_000.0, 0.0, 0.0])];
        let meshes = [MeshBox { min: [0.0; 3], max: [100.0, 100.0, 50.0] }];
        let v = build(&rows, Some(1), &meshes, None, &[], 1);
        assert_eq!(v["bounds"]["max"][0], 50_000.0);
        assert_eq!(v["meshBounds"], json!({"min": [0.0, 0.0, 0.0], "max": [100.0, 100.0, 50.0]}));
        assert_eq!(build(&rows, Some(0), &[], None, &[], 1)["meshBounds"], Value::Null);
    }

    #[test]
    fn build_counts_bounds_spawns_and_query() {
        let rows = vec![
            row(1, "info_player_terrorist", "", [0.0, 0.0, 0.0]),
            row(2, "func_buyzone", "BZ_A", [100.0, -50.0, 8.0]),
            row(3, "func_buyzone", "bz_b", [10.0, 10.0, 10.0]),
        ];
        let v = build(&rows, None, &[], Some("buy"), &["nodeID", "targetname"], 1);
        assert_eq!(v["counts"], json!({"entities": 3, "classes": 2, "meshes": null}));
        assert_eq!(v["classes"], json!([["func_buyzone", 2], ["info_player_terrorist", 1]]));
        assert_eq!(v["otherClasses"], 0);
        assert_eq!(v["spawnsTruncated"], false);
        assert_eq!(v["bounds"], json!({"min": [0.0, -50.0, 0.0], "max": [100.0, 10.0, 10.0]}));
        assert_eq!(v["spawns"].as_array().unwrap().len(), 1);
        assert_eq!(v["matched"], 2);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["items"], json!([{"nodeID": 2, "targetname": "BZ_A"}]));
    }

    #[test]
    fn classes_are_biggest_first_capped_and_spawns_flag_truncation() {
        let mut rows = Vec::new();
        // 45 classes: class_00 has 1 entity, class_01 has 2, ... so the order is the reverse of
        // the alphabetical one
        for c in 0..45 {
            for i in 0..=c {
                rows.push(row(i as i64, &format!("class_{c:02}"), "", [0.0; 3]));
            }
        }
        for i in 0..SPAWN_CAP + 3 {
            rows.push(row(1000 + i as i64, "info_player_counterterrorist", "", [1.0; 3]));
        }
        let v = build(&rows, None, &[], None, &FIELDS, 1);
        let classes = v["classes"].as_array().unwrap();
        assert_eq!(classes.len(), CLASS_CAP);
        // the spawns class has 43 entities: it ties class_42, which sorts first by name
        assert_eq!(classes[0], json!(["class_44", 45]));
        assert_eq!(classes[1], json!(["class_43", 44]));
        assert_eq!(classes[2], json!(["class_42", 43]));
        assert_eq!(classes[3], json!(["info_player_counterterrorist", 43]));
        let counts: Vec<u64> = classes.iter().map(|c| c[1].as_u64().unwrap()).collect();
        assert!(counts.windows(2).all(|w| w[0] >= w[1]), "{counts:?}");
        assert_eq!(v["counts"]["classes"], 46);
        assert_eq!(v["otherClasses"], 6);
        assert_eq!(v["spawns"].as_array().unwrap().len(), SPAWN_CAP);
        assert_eq!(v["spawnsTruncated"], true);
    }

    #[test]
    fn args_are_validated() {
        assert!(parse_args(&json!({"fields": ["bogus"]})).is_err());
        assert!(parse_args(&json!({"limit": 0})).is_err());
        assert!(parse_args(&json!({"query": ""})).is_err());
        let (q, f, l) = parse_args(&json!({"query": "Buy"})).unwrap();
        assert_eq!((q.as_deref(), f.len(), l), (Some("buy"), 4, 25));
    }
}
