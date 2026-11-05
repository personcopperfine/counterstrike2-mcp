//! The live mesh read: `mesh.get` (bridge m3.12) turned into WORLD geometry.
//!
//! `mesh.get` answers the mesh's **node-local** geometry plus its own `origin`/`angles`/`scales`;
//! every route in this server works on world vertices (the saved-file extraction applies the same
//! transform, [`crate::vmap::mesh::local_to_world`]), so the conversion happens once, here, and
//! the result is a [`MeshRecord`] — the very type the file route produces. `get_mesh`, the rebuild
//! operators, the per-face `set_mesh_material`, `set_trigger_volume` and `get_trigger_volume`
//! therefore see identical vertices, faces and per-face materials whichever route supplied them.
//!
//! The reply's `parentChain` (the node's ancestor classes, nearest first, ending at the world) rides
//! along as the record's `ancestors`, so [`MeshRecord::parent_problem`] refuses a mesh under an
//! instance/prefab/unknown container on this route exactly as it does on the file route: `origin`
//! and `angles` are the node's own, so such a container's transform would be missing from the
//! "world" vertices.
//!
//! Nothing here caches: every call is one `mesh.get`, so an undo, a redo, a Hammer UI edit or
//! another client between two calls is simply visible in the next read.
use crate::geometry::Mesh;
use crate::state::HammerState;
use crate::tools::live_common::expect_doc;
use crate::vmap::mesh::{local_to_world, MeshRecord};
use cs2_mcp_common::bridge::RequestOpts;
use cs2_mcp_common::tools::ToolError;
use serde_json::{json, Value};

/// The bridge method this module needs.
pub(crate) const MESH_GET: &str = "mesh.get";

/// The geometry layer's view of a record: world vertices, loops, per-face materials (`None` = the
/// face has no material, i.e. the base material — the saved file's empty material reads the same).
pub(crate) fn mesh_of(record: &MeshRecord) -> Mesh {
    Mesh {
        vertices: record.vertices.clone(),
        faces: record.faces.clone(),
        face_materials: Some(
            record
                .face_materials
                .iter()
                .map(|material| (!material.is_empty()).then(|| material.clone()))
                .collect(),
        ),
    }
}

/// Whether the connected bridge advertises `mesh.get`: one capabilities refresh when the list says
/// no (the pattern `create_props` uses for its marker), so a bridge restarted into a newer build
/// is noticed. A dead bridge is the bridge's own error.
pub(crate) async fn bridge_has_mesh_get(state: &HammerState) -> Result<bool, ToolError> {
    let mut bridge = state.bridge.lock().await;
    bridge.ensure_connected().await?;
    if bridge.has(MESH_GET) {
        return Ok(true);
    }
    let _ = bridge.refresh_capabilities().await;
    Ok(bridge.has(MESH_GET))
}

/// The mesh `node_id` as the live document holds it, in WORLD space.
///
/// `Ok(None)` = the bridge does not advertise `mesh.get` (an older build): the caller falls back to
/// the saved file. `doc` is the document read the caller already made; its fingerprint goes along
/// as `expectDoc`, so a document that moved since is `-32004 DocumentChanged`, never another
/// document's geometry. A missing or non-mesh node is `MeshNotFound` / `NotAMesh`
/// ([`crate::tools::mesh_live::target_failure`]).
pub(crate) async fn read_live_mesh(
    state: &HammerState,
    deadline: u64,
    doc: &Value,
    node_id: i64,
) -> Result<Option<MeshRecord>, ToolError> {
    if !bridge_has_mesh_get(state).await? {
        return Ok(None);
    }
    let reply = {
        let mut bridge = state.bridge.lock().await;
        bridge
            .request(
                MESH_GET,
                json!({ "nodeID": node_id }),
                RequestOpts {
                    deadline_ms: Some(deadline),
                    expect_doc: Some(expect_doc(doc)),
                    gate: None,
                },
            )
            .await
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => {
            if let Some(refusal) = too_large_failure(&error, node_id) {
                return Err(refusal);
            }
            return Err(crate::tools::mesh_live::target_failure(&error, node_id)
                .unwrap_or_else(|| ToolError::from(error)));
        }
    };
    parse_reply(node_id, &reply.result).map(Some).map_err(|why| {
        ToolError::internal(format!("mesh.get answered a malformed reply for nodeID {node_id}: {why}"))
    })
}

/// The name of the refusal [`too_large_failure`] makes (what callers that can fall back to the
/// saved file look for).
pub(crate) const MESH_TOO_LARGE: &str = "MeshTooLarge";

/// Whether `error` is the "mesh too large for one reply" refusal.
pub(crate) fn is_too_large(error: &ToolError) -> bool {
    error.name == MESH_TOO_LARGE
}

/// The bridge's `-32602 {reason:"mesh too large for ...", vertexCount, faceCount, maxLineBytes}`
/// (the mesh does not fit one reply line, or exceeds the geometry limits) as this server's
/// `MeshTooLarge`: retry-safe, nothing was read, and the counts are in `data`.
fn too_large_failure(
    error: &cs2_mcp_common::bridge::BridgeError,
    node_id: i64,
) -> Option<ToolError> {
    let reason = error.data.get("reason").and_then(Value::as_str)?;
    if error.code != Some(-32602) || !reason.starts_with("mesh too large") {
        return None;
    }
    let count = |key: &str| error.data.get(key).and_then(Value::as_i64);
    let (vertices, faces) = (count("vertexCount"), count("faceCount"));
    let counts = match (vertices, faces) {
        (Some(v), Some(f)) => format!("{v} vertices, {f} faces"),
        _ => "counts not reported".to_string(),
    };
    let limit = count("maxLineBytes")
        .map(|bytes| format!(" (one reply line is limited to {bytes} bytes)"))
        .unwrap_or_default();
    Some(
        ToolError::new(
            MESH_TOO_LARGE,
            MESH_TOO_LARGE,
            format!(
                "the mesh with nodeID {node_id} is too large to read live ({counts}){limit}: \
                 nothing was read or sent. save_map and retry to use the saved .vmap (the file \
                 route works while the document is clean), or work on a smaller mesh."
            ),
        )
        .with_code(-32602)
        .with_data(json!({
            "nodeID": node_id,
            "reason": reason,
            "vertexCount": vertices,
            "faceCount": faces,
            "maxLineBytes": count("maxLineBytes"),
            "geometrySource": "unavailable",
        }))
        .retry_safe(true),
    )
}

/// The bridge's ancestor class (`mesh.get`'s `parentChain`) as the `CMap*` class name the saved
/// file's ancestor walk records, so [`MeshRecord::parent_problem`] judges both routes alike.
fn cmap_class(class: &str) -> String {
    match class {
        "world" => "CMapWorld".into(),
        "group" => "CMapGroup".into(),
        "entity" => "CMapEntity".into(),
        "prefab" => "CMapPrefab".into(),
        "instance" => "CMapInstance".into(),
        other => other.into(),
    }
}

fn vec3(value: Option<&Value>, default: [f64; 3]) -> Result<[f64; 3], String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let items = value.as_array().filter(|items| items.len() == 3).ok_or("a vector is not [x,y,z]")?;
    let mut out = [0.0f64; 3];
    for (axis, item) in items.iter().enumerate() {
        out[axis] = item.as_f64().filter(|x| x.is_finite()).ok_or("a vector holds a non-number")?;
    }
    Ok(out)
}

/// `mesh.get`'s result -> the world-space [`MeshRecord`].
///
/// `vertices` are local: world = origin + R(angles) * (scales * local), the transform the saved-file
/// route applies to its stored vertices. `faceMaterials` index `materials`; `-1` (or an index
/// outside the list) is a face without a material, which reads as the empty string, exactly like
/// the file route's mesh that lists no material.
pub(crate) fn parse_reply(node_id: i64, result: &Value) -> Result<MeshRecord, String> {
    let origin = vec3(result.get("origin"), [0.0; 3])?;
    let angles = vec3(result.get("angles"), [0.0; 3])?;
    let scales = vec3(result.get("scales"), [1.0; 3])?;
    let local: Vec<[f64; 3]> = result
        .get("vertices")
        .and_then(Value::as_array)
        .ok_or("no vertices array")?
        .iter()
        .map(|vertex| vec3(Some(vertex), [0.0; 3]))
        .collect::<Result<_, _>>()?;
    let vertices: Vec<[f64; 3]> =
        local.iter().map(|point| local_to_world(*point, scales, angles, origin)).collect();
    let faces: Vec<Vec<usize>> = result
        .get("faces")
        .and_then(Value::as_array)
        .ok_or("no faces array")?
        .iter()
        .map(|face| {
            face.as_array()
                .ok_or("a face is not an array")?
                .iter()
                .map(|index| {
                    index
                        .as_u64()
                        .map(|index| index as usize)
                        .filter(|index| *index < vertices.len())
                        .ok_or("a face index is out of range")
                })
                .collect::<Result<Vec<usize>, _>>()
        })
        .collect::<Result<_, _>>()?;
    let materials: Vec<String> = match result.get("materials") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string).ok_or("a material is not a string"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("materials is not an array".into()),
    };
    let indices: Vec<i64> = match result.get("faceMaterials") {
        None | Some(Value::Null) => vec![-1; faces.len()],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_i64().ok_or("a face material index is not an int"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("faceMaterials is not an array".into()),
    };
    if indices.len() != faces.len() {
        return Err(format!("{} faceMaterials for {} faces", indices.len(), faces.len()));
    }
    let face_materials = indices
        .iter()
        .map(|index| {
            usize::try_from(*index)
                .ok()
                .and_then(|index| materials.get(index))
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    Ok(MeshRecord {
        id: Some(node_id),
        origin: Some(origin),
        angles: Some(angles),
        scales: Some(scales),
        vertices,
        faces,
        face_materials,
        materials,
        // the ancestor classes (nearest first, ending at the world); a reply without the member
        // (not a m3.12+ build) leaves it unproven, which `parent_problem` refuses
        ancestors: match result.get("parentChain") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|item| item.as_str().map(cmap_class).ok_or("a parentChain entry is not a string"))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_converted_to_world_with_the_file_routes_transform() {
        let result = json!({
            "nodeID": 5, "origin": [10, 20, 30], "angles": [0, 90, 0], "scales": [2, 1, 1],
            "vertices": [[1, 0, 0], [0, 1, 0], [0, 0, 1]],
            "faces": [[0, 1, 2]], "faceMaterials": [0], "materials": ["m/a.vmat"],
        });
        let record = parse_reply(5, &result).expect("parses");
        // scale x2 -> (2,0,0); yaw 90 maps +x onto +y -> (0,2,0); then + origin
        let want = [10.0, 22.0, 30.0];
        for (got, want) in record.vertices[0].iter().zip(want) {
            assert!((got - want).abs() < 1e-9, "{:?}", record.vertices[0]);
        }
        assert_eq!(record.face_materials, vec!["m/a.vmat".to_string()]);
        assert_eq!(record.faces, vec![vec![0, 1, 2]]);
        assert_eq!(record.origin, Some([10.0, 20.0, 30.0]));
    }

    #[test]
    fn a_missing_material_reads_empty_and_a_malformed_reply_is_refused() {
        let ok = json!({"vertices": [[0,0,0],[1,0,0],[0,1,0]], "faces": [[0,1,2]],
                        "faceMaterials": [-1], "materials": []});
        let record = parse_reply(1, &ok).expect("parses");
        assert_eq!(record.face_materials, vec![String::new()]);
        assert!(mesh_of(&record).face_materials.unwrap()[0].is_none());

        for bad in [
            json!({"faces": []}),
            json!({"vertices": [[0,0,0]], "faces": [[0,1]]}),
            json!({"vertices": [[0,0]], "faces": []}),
            json!({"vertices": [[0,0,0]], "faces": [[0]], "faceMaterials": []}),
        ] {
            assert!(parse_reply(1, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_parent_chain_becomes_the_records_ancestors() {
        let base = |chain: Value| {
            json!({"vertices": [], "faces": [], "faceMaterials": [], "materials": [],
                   "parentChain": chain})
        };
        let record = parse_reply(1, &base(json!(["group", "entity", "world"]))).expect("parses");
        assert_eq!(
            record.ancestors,
            Some(vec!["CMapGroup".to_string(), "CMapEntity".to_string(), "CMapWorld".to_string()])
        );
        assert!(record.parent_problem().is_none());
        let record = parse_reply(1, &base(json!(["instance", "world"]))).expect("parses");
        assert!(record.parent_problem().is_some(), "an instance is not a proven container");
        // a reply without the member is unproven
        let record = parse_reply(1, &json!({"vertices": [], "faces": []})).expect("parses");
        assert!(record.parent_problem().is_some());
        assert!(parse_reply(1, &base(json!([1]))).is_err());
    }

    #[test]
    fn an_empty_mesh_is_an_empty_record() {
        let record = parse_reply(
            3,
            &json!({"vertices": [], "faces": [], "faceMaterials": [], "materials": []}),
        )
        .expect("parses");
        assert!(record.vertices.is_empty() && record.faces.is_empty());
    }
}
