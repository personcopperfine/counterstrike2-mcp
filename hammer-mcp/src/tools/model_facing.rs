//! The `facing` block of `get_model_info` (streamline P5, ti_convoy FIXME 21): which horizontal
//! direction a compiled model's front points along, decided ONLY from the attachments in the
//! file: names that mark a front or a rear (`front`/`rear`/`back`, wheel `fl fr rl rr`,
//! `headlight`/`taillight`) and their positions. The box's long axis says nothing about the sign
//! (a car's long axis is the same whichever way it drives), so it is reported separately as
//! `longAxis` and never used to decide. No usable evidence is `forward:"unknown"`, never a guess.
//!
//! Positions are the first influence's offset, which is in that BONE's frame, not the model's.
//! It is read as a model-frame position only when the attachment has exactly one influence that
//! is not root-transform relative AND the bone and every parent up to the root have identity
//! rest rotation and unit scale (`AttachmentPoint::model_offset`, from `m_modelSkeleton`);
//! otherwise that attachment casts no vote and `basis` says why. Two attachments are also only
//! compared when they share the bone. A pair whose front-minus-rear vector is not clearly along
//! x or y (dominant axis at least twice the other, at least 1 unit) is unusable too.
use crate::resource::model::{AttachmentPoint, ModelInfo};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Front,
    Rear,
}

const FRONT_TOKENS: &[&str] = &["front", "fl", "fr", "headlight", "headlights", "headlamp"];
const REAR_TOKENS: &[&str] = &["rear", "back", "rl", "rr", "taillight", "taillights", "tail"];

/// Front or rear by the name's `_`/`-`/space separated tokens; a name with both (or neither) has
/// no role.
fn role_of(name: &str) -> Option<Role> {
    let lower = name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let front = tokens.iter().any(|t| FRONT_TOKENS.contains(t));
    let rear = tokens.iter().any(|t| REAR_TOKENS.contains(t));
    match (front, rear) {
        (true, false) => Some(Role::Front),
        (false, true) => Some(Role::Rear),
        _ => None,
    }
}

fn mean(points: &[[f32; 3]]) -> [f64; 3] {
    let n = points.len() as f64;
    let mut sum = [0.0f64; 3];
    for p in points {
        for (s, v) in sum.iter_mut().zip(p) {
            *s += f64::from(*v);
        }
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// The decision for one bone: `Ok(direction)` or `Err(why it cannot be used)`.
fn bone_vote(bone: &str, front: &[Placed], rear: &[Placed]) -> Result<&'static str, String> {
    let f = mean(&front.iter().map(|p| p.offset).collect::<Vec<_>>());
    let r = mean(&rear.iter().map(|p| p.offset).collect::<Vec<_>>());
    let (dx, dy) = (f[0] - r[0], f[1] - r[1]);
    let (major, minor, plus, minus) = if dx.abs() >= dy.abs() {
        (dx, dy, "+x", "-x")
    } else {
        (dy, dx, "+y", "-y")
    };
    if major.abs() < 1.0 || major.abs() < 2.0 * minor.abs() {
        return Err(format!(
            "bone {bone}: front minus rear is ({dx:.1}, {dy:.1}), not clearly along x or y"
        ));
    }
    Ok(if major > 0.0 { plus } else { minus })
}

/// An attachment with a usable model-frame position.
struct Placed<'a> {
    name: &'a str,
    offset: [f32; 3],
}

fn names(points: &[Placed]) -> Vec<String> {
    points.iter().map(|p| p.name.to_string()).collect()
}

/// The `facing` object: `{forward, basis, confidence, longAxis?, evidence?, next?}`.
pub(crate) fn facing(info: &ModelInfo) -> Value {
    let long_axis = info.render_bounds.map(|(min, max)| {
        if (max[0] - min[0]) >= (max[1] - min[1]) { "x" } else { "y" }
    });
    let unknown = |basis: String| {
        let mut out = json!({
            "forward": "unknown",
            "basis": basis,
            "confidence": "none",
            "next": {
                "tool": "model_render",
                "why": "open the model (model_open) and render it from two sides to see which way the front points",
            },
        });
        if let Some(axis) = long_axis {
            out["longAxis"] = json!(axis);
        }
        out
    };
    if info.attachment_points.is_empty() {
        return unknown("the model has no attachments; the facing is decided only from attachment names and positions".into());
    }
    let roled: Vec<(Role, &AttachmentPoint)> = info
        .attachment_points
        .iter()
        .filter_map(|p| role_of(&p.name).map(|r| (r, p)))
        .collect();
    if roled.is_empty() {
        return unknown(format!(
            "none of the {} attachment name(s) marks a front or rear (front/rear/back, fl/fr/rl/rr, headlight/taillight): {}",
            info.attachment_points.len(),
            info.attachment_points.iter().take(8).map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    // group the usable ones (offset + bone) by bone
    let mut by_bone: BTreeMap<&str, (Vec<Placed>, Vec<Placed>)> = BTreeMap::new();
    let mut unusable: Vec<String> = Vec::new();
    for (role, p) in &roled {
        let placed = match (&p.bone, p.model_offset()) {
            (Some(bone), Ok(offset)) => Ok((bone.as_str(), offset)),
            (None, Ok(_)) => Err(format!("{} has no bone", p.name)),
            (_, Err(why)) => Err(why),
        };
        match placed {
            Ok((bone, offset)) => {
                let slot = by_bone.entry(bone).or_default();
                let entry = Placed { name: &p.name, offset };
                match role {
                    Role::Front => slot.0.push(entry),
                    Role::Rear => slot.1.push(entry),
                }
            }
            Err(why) => unusable.push(why),
        }
    }
    let mut votes: Vec<(&'static str, String)> = Vec::new();
    for (bone, (front, rear)) in &by_bone {
        if front.is_empty() || rear.is_empty() {
            let only = if front.is_empty() { &rear } else { &front };
            unusable.push(format!(
                "bone {bone}: only {} marked {} (no counterpart on the same bone)",
                names(only).join(", "),
                if front.is_empty() { "rear" } else { "front" }
            ));
            continue;
        }
        match bone_vote(bone, front, rear) {
            Ok(direction) => votes.push((
                direction,
                format!(
                    "bone {bone}: front [{}] is {} of rear [{}]",
                    names(front).join(", "),
                    direction,
                    names(rear).join(", ")
                ),
            )),
            Err(why) => unusable.push(why),
        }
    }
    let evidence: Vec<&str> = votes.iter().map(|(_, e)| e.as_str()).collect();
    if votes.is_empty() {
        return unknown(format!(
            "attachments with front/rear names exist but cannot be compared: {}",
            unusable.join("; ")
        ));
    }
    let first = votes[0].0;
    if votes.iter().any(|(d, _)| *d != first) {
        return unknown(format!("the attachments disagree: {}", evidence.join("; ")));
    }
    let mut out = json!({
        "forward": first,
        "basis": format!(
            "attachment positions (same-bone front minus rear): {}{}",
            evidence.join("; "),
            if unusable.is_empty() { String::new() } else { format!(" (not used: {})", unusable.join("; ")) }
        ),
        "confidence": if votes.len() >= 2 { "high" } else { "medium" },
    });
    if let Some(axis) = long_axis {
        out["longAxis"] = json!(axis);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(name: &str, bone: &str, off: [f32; 3]) -> AttachmentPoint {
        AttachmentPoint {
            name: name.into(),
            bone: Some(bone.into()),
            offset: Some(off),
            influences: Some(1),
            root_transform: false,
            bone_aligned: Some(true),
        }
    }

    fn info(points: Vec<AttachmentPoint>) -> ModelInfo {
        ModelInfo {
            name: None,
            hull: None,
            view: None,
            mass: None,
            surface_property: None,
            keyvalues_text: None,
            skins: vec![],
            bodygroups: vec![],
            sequences: vec![],
            attachments: points.iter().map(|p| p.name.clone()).collect(),
            attachment_points: points,
            has_physics: None,
            render_bounds: Some(([-100.0, -40.0, 0.0], [100.0, 40.0, 60.0])),
            physics_bounds: None,
            bones: vec![],
            blocks: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn front_and_rear_wheels_decide_the_sign() {
        let f = facing(&info(vec![
            pt("wheel_fl", "root", [-60.0, 30.0, 10.0]),
            pt("wheel_fr", "root", [-60.0, -30.0, 10.0]),
            pt("wheel_rl", "root", [70.0, 30.0, 10.0]),
            pt("wheel_rr", "root", [70.0, -30.0, 10.0]),
        ]));
        assert_eq!(f["forward"], "-x", "{f}");
        assert_eq!(f["confidence"], "medium");
        assert_eq!(f["longAxis"], "x");
        let f = facing(&info(vec![
            pt("car_front_wheelset", "root", [80.0, 0.0, 0.0]),
            pt("car_rear_wheelset", "root", [-80.0, 0.0, 0.0]),
        ]));
        assert_eq!(f["forward"], "+x", "{f}");
    }

    #[test]
    fn two_bones_that_agree_are_high_and_that_disagree_are_unknown() {
        let agree = facing(&info(vec![
            pt("front", "a", [0.0, 50.0, 0.0]),
            pt("rear", "a", [0.0, -50.0, 0.0]),
            pt("headlight", "b", [0.0, 10.0, 0.0]),
            pt("taillight", "b", [0.0, -10.0, 0.0]),
        ]));
        assert_eq!((agree["forward"].as_str(), agree["confidence"].as_str()), (Some("+y"), Some("high")));
        let clash = facing(&info(vec![
            pt("front", "a", [50.0, 0.0, 0.0]),
            pt("rear", "a", [-50.0, 0.0, 0.0]),
            pt("headlight", "b", [-10.0, 0.0, 0.0]),
            pt("taillight", "b", [10.0, 0.0, 0.0]),
        ]));
        assert_eq!(clash["forward"], "unknown", "{clash}");
        assert!(clash["basis"].as_str().unwrap().contains("disagree"));
    }

    #[test]
    fn no_evidence_is_unknown_with_a_render_next_and_never_the_long_axis() {
        for points in [vec![], vec![pt("handle", "root", [1.0, 0.0, 0.0])]] {
            let f = facing(&info(points));
            assert_eq!(f["forward"], "unknown");
            assert_eq!(f["confidence"], "none");
            assert_eq!(f["next"]["tool"], "model_render");
            assert_eq!(f["longAxis"], "x");
        }
    }

    #[test]
    fn unusable_evidence_says_why() {
        // different bones, a lone front, a diagonal and a missing offset: none decides
        let f = facing(&info(vec![pt("front", "a", [5.0, 0.0, 0.0]), pt("rear", "b", [-5.0, 0.0, 0.0])]));
        assert_eq!(f["forward"], "unknown");
        assert!(f["basis"].as_str().unwrap().contains("no counterpart"), "{f}");
        let f = facing(&info(vec![pt("front", "a", [50.0, 50.0, 0.0]), pt("rear", "a", [0.0, 0.0, 0.0])]));
        assert!(f["basis"].as_str().unwrap().contains("not clearly"), "{f}");
        let mut p = pt("front", "a", [0.0; 3]);
        p.offset = None;
        let f = facing(&info(vec![p, pt("rear", "a", [0.0; 3])]));
        assert!(f["basis"].as_str().unwrap().contains("no offset"), "{f}");
    }

    /// The offsets are in the bone's frame: a rotated bone, several influences, a root-transform
    /// offset or an unknown skeleton entry cast no vote, whatever the numbers say.
    #[test]
    fn a_bone_frame_that_may_differ_from_the_model_frame_gives_no_vote() {
        let pair = |tweak: &dyn Fn(&mut AttachmentPoint)| {
            let mut front = pt("front", "a", [50.0, 0.0, 0.0]);
            let rear = pt("rear", "a", [-50.0, 0.0, 0.0]);
            tweak(&mut front);
            facing(&info(vec![front, rear]))
        };
        assert_eq!(pair(&|_| {})["forward"], "+x");
        let yawed = pair(&|p| p.bone_aligned = Some(false));
        assert_eq!(yawed["forward"], "unknown", "{yawed}");
        assert!(yawed["basis"].as_str().unwrap().contains("rotated or scaled"), "{yawed}");
        let multi = pair(&|p| p.influences = Some(2));
        assert_eq!(multi["forward"], "unknown");
        assert!(multi["basis"].as_str().unwrap().contains("influences"));
        assert_eq!(pair(&|p| p.root_transform = true)["forward"], "unknown");
        let no_skeleton = pair(&|p| p.bone_aligned = None);
        assert_eq!(no_skeleton["forward"], "unknown");
        assert!(no_skeleton["basis"].as_str().unwrap().contains("skeleton"));
    }

    #[test]
    fn roles_need_whole_tokens() {
        assert_eq!(role_of("Wheel_FL"), Some(Role::Front));
        assert_eq!(role_of("backdrop"), None);
        assert_eq!(role_of("front_back"), None);
        assert_eq!(role_of("flag"), None);
    }
}
