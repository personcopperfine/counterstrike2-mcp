//! The volume/material argument parsing shared by the trigger tools (review-rust.md D1:
//! `create_trigger`/`tie_to_entity` in `triggers_live.rs` and `set_trigger_volume` in
//! `trigger_volume.rs` took the same ~200 lines twice). `pub(crate)`; behaviour is the two
//! modules' previous local code, unchanged.
//!
//! Two shapes exist on purpose:
//! * `triggers_live.rs` builds the volume RAW and hands it to `mesh_live::send_geometry`, which
//!   rounds the vertices, runs the bridge's coordinate gate and the face gates (on the rounded
//!   numbers and on the bridge's f32 widening) and enforces the request byte budget — the single
//!   implementation `create_mesh` uses (review A1). Its local `round4`/`as_float`/
//!   `check_geometry` copies are gone since A1.
//! * `trigger_volume.rs` rounds and face-checks the volume in the argument phase (before the
//!   first bridge request — the entity read has already gone out there) and then sends the same
//!   numbers through `send_geometry` again; it uses [`volume_arg`] (round + check).
use crate::geometry::{self, GeometryError, Vec3};
use cs2_mcp_common::bridge::BridgeError;
use cs2_mcp_common::tools::ToolError;
use serde_json::Value;

use super::live_common::invalid;

/// The trigger material the trigger tools default to (the class convention of `core/base.fgd`;
/// the fallback when the FGD carries no `auto_apply_material`).
pub(crate) const TRIGGER_MATERIAL: &str = "materials/tools/toolstrigger.vmat";
/// A `box` corner is within this magnitude (the bridge's own node-coordinate range).
pub(crate) const MAX_BOX_COORD: f64 = 1_048_576.0;
/// A `box` smaller than this on any axis is refused (a zero-thickness volume is nothing Hammer
/// can build).
pub(crate) const MIN_BOX_SIZE: f64 = 1.0;
/// The material path buffer the bridge uses (`char mats[64][260]`), like the other tools.
pub(crate) const MAX_MATERIAL_BYTES: usize = 259;

/// `material?`: a non-empty path of at most [`MAX_MATERIAL_BYTES`] bytes without control
/// characters (`mesh_live::material_arg`'s rules). `None` = the argument was not given.
pub(crate) fn material_arg(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(invalid(format!(
            "{key} must be a material path (e.g. {TRIGGER_MATERIAL}), got {value}"
        )));
    };
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(format!(
            "{key} must be a material path (e.g. {TRIGGER_MATERIAL}), got an empty string"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{key}: a material path cannot contain control characters ({text:?})"
        )));
    }
    if text.len() > MAX_MATERIAL_BYTES {
        return Err(invalid(format!(
            "{key}: the material path is {} bytes, the bridge's buffer holds {MAX_MATERIAL_BYTES}",
            text.len()
        )));
    }
    Ok(Some(text.to_string()))
}

/// One `box` corner: `[x, y, z]` of finite numbers within [`MAX_BOX_COORD`].
pub(crate) fn box_corner(label: &str, value: &Value) -> Result<Vec3, ToolError> {
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid(format!(
            "{label} must be [x, y, z] (three numbers), got {value}"
        )));
    };
    let mut out: Vec3 = [0.0; 3];
    for (axis, value) in axes.iter().enumerate() {
        let Some(number) = value.as_f64().filter(|number| number.is_finite()) else {
            return Err(invalid(format!(
                "{label}[{axis}] must be a finite number, got {value}"
            )));
        };
        if number.abs() > MAX_BOX_COORD {
            return Err(invalid(format!(
                "{label}[{axis}] is {number}: |coordinate| <= {MAX_BOX_COORD} (the bridge's own \
                 node range)"
            )));
        }
        out[axis] = number;
    }
    Ok(out)
}

/// `box {min, max}`: finite corners within [`MAX_BOX_COORD`], `min < max` and at least
/// [`MIN_BOX_SIZE`] on every axis, built through [`crate::geometry`] (the same builder
/// `create_block` uses). The vertices are handed back UNROUNDED: the callers pass them to
/// `mesh_live::send_geometry`, which rounds them and runs the coordinate/face gates itself
/// (review A1).
pub(crate) fn box_volume(value: &Value) -> Result<(Vec<Vec3>, Vec<Vec<usize>>), ToolError> {
    let Some(object) = value.as_object() else {
        return Err(invalid("box must be an object {min, max}"));
    };
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "min" | "max"))
    {
        return Err(invalid(format!(
            "box has the unknown member {unknown:?}: it takes min and max"
        )));
    }
    let (Some(min), Some(max)) = (object.get("min"), object.get("max")) else {
        return Err(invalid("box needs both min and max ([x, y, z] each)"));
    };
    let min = box_corner("box.min", min)?;
    let max = box_corner("box.max", max)?;
    for (axis, (lo, hi)) in min.iter().zip(max.iter()).enumerate() {
        if hi - lo < MIN_BOX_SIZE {
            return Err(invalid(format!(
                "box is {:.6} units on axis {axis} (min {lo} / max {hi}): every side must be at \
                 least {MIN_BOX_SIZE} unit, so min < max and the volume has a real size",
                hi - lo
            )));
        }
    }
    let primitive = geometry::r#box(min, max).map_err(geometry_invalid)?;
    Ok((primitive.mesh.vertices, primitive.mesh.faces))
}

/// `geometry {vertices, faces}`: the `mesh.create` rules ([`geometry::validate`]). The vertices
/// are handed back UNROUNDED — the rounding and the face gates are
/// `mesh_live::send_geometry`'s (review A1).
pub(crate) fn geometry_volume(value: &Value) -> Result<(Vec<Vec3>, Vec<Vec<usize>>), ToolError> {
    let Some(object) = value.as_object() else {
        return Err(invalid("geometry must be an object {vertices, faces}"));
    };
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "vertices" | "faces"))
    {
        return Err(invalid(format!(
            "geometry has the unknown member {unknown:?}: it takes vertices and faces"
        )));
    }
    let mesh = geometry::validate(object.get("vertices"), object.get("faces"), None, None)
        .map_err(geometry_invalid)?;
    Ok((mesh.vertices, mesh.faces))
}

/// Exactly one volume form (`box` | `geometry`), ROUNDED and face-checked —
/// `set_trigger_volume`'s two forms (its mesh always exists, `meshNodeID` only selects among the
/// entity's children). Two or none are argument errors, like `create_trigger`'s; the vertices are
/// rounded the way `send_geometry` sends them and every face must be a real polygon on exactly
/// the numbers the bridge reads (the local gate that runs BEFORE the first request there).
pub(crate) fn volume_arg(args: &Value) -> Result<(Vec<Vec3>, Vec<Vec<usize>>), ToolError> {
    let box_ = args.get("box").filter(|value| !value.is_null());
    let geometry_ = args.get("geometry").filter(|value| !value.is_null());
    let given = [box_, geometry_].iter().filter(|v| v.is_some()).count();
    if given != 1 {
        return Err(invalid(format!(
            "exactly one volume is required: `box` (min/max) or `geometry` (vertices/faces) — \
             got {given} of 2"
        )));
    }
    let (vertices, faces) = match (box_, geometry_) {
        (Some(value), _) => box_volume(value)?,
        (_, Some(value)) => geometry_volume(value)?,
        _ => unreachable!("exactly one volume was checked above"),
    };
    let vertices = round4(&vertices);
    check_geometry(&vertices, &faces)?;
    Ok((vertices, faces))
}

/// A [`GeometryError`] is an argument problem (`mesh_live::geometry_invalid`): `InvalidArguments`,
/// nothing sent.
pub(crate) fn geometry_invalid(error: GeometryError) -> ToolError {
    invalid(format!("geometry: {}", error.0))
}

/// The vertices rounded the way `send_geometry` sends them (`vmap::mesh::round4`, the POC's
/// `round(c, 4)`; `-0.0` becomes `0.0`).
pub(crate) fn round4(vertices: &[Vec3]) -> Vec<Vec3> {
    vertices
        .iter()
        .map(|point| {
            point.map(|c| {
                let rounded = crate::vmap::mesh::round4(c);
                if rounded == 0.0 {
                    0.0
                } else {
                    rounded
                }
            })
        })
        .collect()
}

/// The f32 widening the bridge applies to every coordinate it parses (`geom_parse`).
pub(crate) fn as_float(point: Vec3) -> Vec3 {
    point.map(|c| c as f32 as f64)
}

/// The volume faces must be real polygons on exactly the numbers the bridge reads: every
/// coordinate inside the bridge's range and every face non-degenerate both on the rounded f64s
/// and after the bridge's float widening.
pub(crate) fn check_geometry(vertices: &[Vec3], faces: &[Vec<usize>]) -> Result<(), ToolError> {
    for (index, point) in vertices.iter().enumerate() {
        if point.iter().any(|c| c.abs() >= geometry::MAX_COORD) {
            return Err(invalid(format!(
                "vertices[{index}] is outside +-{} (the bridge refuses it)",
                geometry::MAX_COORD
            )));
        }
    }
    let floats: Vec<Vec3> = vertices.iter().copied().map(as_float).collect();
    for (index, face) in faces.iter().enumerate() {
        geometry::face_normal(vertices, face).map_err(|error| {
            invalid(format!(
                "faces[{index}]: {error} (Hammer would create nothing for a zero-area face)"
            ))
        })?;
        geometry::face_normal(&floats, face).map_err(|error| {
            invalid(format!(
                "faces[{index}]: {error} once the bridge widens the coordinates to float (Hammer \
                 would create nothing for it)"
            ))
        })?;
    }
    Ok(())
}

/// The `reason` the bridge put into an error's data (`""` when it sent none).
pub(crate) fn reason_of(error: &BridgeError) -> String {
    error
        .data
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
