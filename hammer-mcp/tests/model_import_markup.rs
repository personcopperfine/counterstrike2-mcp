//! Player import: the reference agent's weapon attachments and hitboxes (`import::retarget::agent`).
//!
//! The agent fixture is `common::agent_vmdl_c` (numbers and names from the real `ctm_sas.vmdl_c`);
//! the `#[ignore]`d tests read the installed game instead
//! (`cargo test -p hammer-mcp --test model_import_markup -- --ignored`, `HAMMER_MCP_GAME_PAK` overrides the pak).
mod common;

use common::agent_data::{ATTACHMENTS, BONES, HITBOXES};
use common::humanoid::{self, Opts};
use hammer_mcp::import::retarget::agent::{self, Markup, Shape};
use hammer_mcp::import::retarget::mapping::RigHint;
use hammer_mcp::import::retarget::skeleton::{self, NmSkeleton};
use hammer_mcp::import::retarget::PlayerOptions;
use hammer_mcp::import::{self, ImportOptions, ImportPlan, Kind};
use serde_json::{json, Value};
use std::path::Path;

fn skel() -> NmSkeleton {
    skeleton::from_resource_bytes(&common::vnmskel_c()).expect("fixture skeleton")
}

fn markup() -> Markup {
    agent::from_resource_bytes(&common::agent_vmdl_c()).expect("fixture agent")
}

fn plan(markup: Option<Markup>) -> ImportPlan {
    let h = humanoid::build(&Opts::default());
    let player = PlayerOptions {
        skeleton: skel(),
        bone_map: vec![],
        rig: RigHint::Auto,
        markup: markup.map(|m| (agent::DEFAULT_AGENT.to_string(), m)),
    };
    let opts = ImportOptions {
        name: "hero".into(),
        kind: Kind::Player,
        scale: None,
        up: None,
        generate_materials: true,
        existing: None,
        physics: None,
        player: Some(player),
    };
    import::plan(Path::new("hero.glb"), h.fixture.glb(), &opts).expect("plan")
}

/// The root node's children as JSON.
fn lists(plan: &ImportPlan) -> Vec<Value> {
    let root = cs2_kv3::to_json(&cs2_kv3::Value::Object(plan.vmdl.root().clone()));
    root["children"].as_array().unwrap().clone()
}

fn list<'a>(lists: &'a [Value], class: &str) -> &'a Value {
    lists.iter().find(|n| n["_class"] == class).unwrap_or_else(|| panic!("no {class}"))
}

fn close(a: &Value, b: [f64; 3], tol: f64, what: &str) {
    let a: Vec<f64> = a.as_array().unwrap_or_else(|| panic!("{what}: {a}")).iter().map(|x| x.as_f64().unwrap()).collect();
    assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol), "{what}: {a:?} vs {b:?}");
}

/// ValveResourceFormat `EulerAnglesToQuaternion` (the inverse of `quat_to_angles`).
fn angles_to_quat(a: [f64; 3]) -> [f64; 4] {
    let h = |d: f64| (d.to_radians() * 0.5).sin_cos();
    let ((sp, cp), (sy, cy), (sr, cr)) = (h(a[0]), h(a[1]), h(a[2]));
    [sr * cp * cy - cr * sp * sy, cr * sp * cy + sr * cp * sy, cr * cp * sy - sr * sp * cy, cr * cp * cy + sr * sp * sy]
}

fn same_rotation(a: [f64; 4], b: [f64; 4]) -> bool {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    dot.abs() > 1.0 - 1e-6
}

#[test]
fn the_compiled_agent_layout_is_read() {
    let m = markup();
    assert_eq!(m.attachments.len(), ATTACHMENTS.len());
    let weapon = m.attachments.iter().find(|a| a.name == "weapon").unwrap();
    assert_eq!(weapon.influences.len(), 1);
    assert_eq!(weapon.influences[0].bone, "wpn");
    let center = m.attachments.iter().find(|a| a.name == "weapon_center").unwrap();
    assert!(center.influences[0].root_transform);
    assert_eq!(center.influences[0].offset, [0.0, 0.0, 40.0]);
    assert_eq!(m.hitbox_sets.len(), 1);
    assert_eq!(m.hitbox_sets[0].name, "cstrike");
    assert_eq!(m.hitbox_sets[0].hitboxes.len(), HITBOXES.len());
    let head = &m.hitbox_sets[0].hitboxes[0];
    assert_eq!((head.name.as_str(), head.bone.as_str(), head.shape, head.group), ("head_0", "head_0", Shape::Capsule, 1));
    assert_eq!(head.surface, "playerflesh");
    assert_eq!(m.bones.len(), BONES.len());
}

#[test]
fn a_file_without_mdat_is_a_typed_error() {
    let bytes = common::model_vmdl_c(&[("DATA", common::kv_object(vec![("m_name", common::kv_text("x.vmdl"))]))]);
    let e = agent::from_resource_bytes(&bytes).unwrap_err();
    assert!(e.to_string().contains("MDAT"), "{e}");
}

#[test]
fn every_attachment_lands_on_a_lowercase_import_bone() {
    let fitted = agent::fit(&markup(), &skel());
    assert!(fitted.dropped.is_empty(), "{:?}", fitted.dropped);
    assert_eq!(fitted.attachments.len(), ATTACHMENTS.len());
    let s = skel();
    for a in &fitted.attachments {
        for p in &a.influences {
            if a.name == "weapon_center" {
                assert_eq!(p.bone, "", "root transform = model origin");
                continue;
            }
            assert_eq!(p.bone, p.bone.to_ascii_lowercase());
            assert!(s.names.iter().any(|n| n.eq_ignore_ascii_case(&p.bone)), "{} on {}", a.name, p.bone);
        }
    }
    let hand = fitted.attachments.iter().find(|a| a.name == "weapon_hand_r").unwrap();
    assert_eq!(hand.influences[0].bone, "hand_r");
    assert_eq!(hand.influences[0].offset, [-2.5999999046325684, -1.399999976158142, 0.0]);
    // hitboxes copied unscaled, bones lowercase
    let set = &fitted.hitbox_sets[0];
    assert_eq!(set.hitboxes.len(), HITBOXES.len());
    for (h, row) in set.hitboxes.iter().zip(HITBOXES) {
        assert_eq!(h.bone, row.1.to_ascii_lowercase());
        assert_eq!((h.radius, h.min, h.max, h.group), (row.3, row.4, row.5, row.7));
    }
}

#[test]
fn a_bone_the_skeleton_lacks_is_folded_into_its_ancestor() {
    let fitted = agent::fit(&markup(), &skel());
    // jiggle_primary (agent only) -> spine_3: offset = pos, rotation = rot (the attachment is identity on it)
    let primary = fitted.attachments.iter().find(|a| a.name == "primary").unwrap();
    let jiggle = BONES.iter().find(|b| b.0 == "jiggle_primary").unwrap();
    assert_eq!(BONES[jiggle.1 as usize].0, "spine_3");
    let p = &primary.influences[0];
    assert_eq!(p.bone, "spine_3");
    for k in 0..3 {
        assert!((p.offset[k] - jiggle.2[k]).abs() < 1e-9, "{:?}", p.offset);
    }
    assert!(same_rotation(p.rotation, jiggle.3), "{:?}", p.rotation);
    assert_eq!(fitted.moved, vec![("primary".to_string(), "jiggle_primary".to_string(), "spine_3".to_string())]);
}

#[test]
fn folding_composes_offset_and_rotation() {
    let mut m = markup();
    // a non-zero attachment on jiggle_primary: spine_3 frame = pos + R * offset, rotation R * q
    let a = m.attachments.iter_mut().find(|a| a.name == "primary").unwrap();
    a.influences[0].offset = [1.0, 2.0, 3.0];
    let q = angles_to_quat([10.0, 20.0, 30.0]);
    a.influences[0].rotation = q;
    let fitted = agent::fit(&m, &skel());
    let p = &fitted.attachments.iter().find(|a| a.name == "primary").unwrap().influences[0];
    let jiggle = BONES.iter().find(|b| b.0 == "jiggle_primary").unwrap();
    // rotate [1,2,3] by the jiggle rotation through the rotation matrix
    let [x, y, z, w] = jiggle.3;
    let m3 = [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ];
    for (k, row) in m3.iter().enumerate() {
        let want = jiggle.2[k] + row[0] + 2.0 * row[1] + 3.0 * row[2];
        // the agent's quaternions are f32 data, unit only to ~1e-7
        assert!((p.offset[k] - want).abs() < 1e-5, "{k}: {:?}", p.offset);
    }
    // (R * q) applied to +X equals R applied to (q applied to +X)
    let rot = |q: [f64; 4], v: [f64; 3]| {
        let [x, y, z, w] = q;
        let t = [2.0 * (y * v[2] - z * v[1]), 2.0 * (z * v[0] - x * v[2]), 2.0 * (x * v[1] - y * v[0])];
        [v[0] + w * t[0] + y * t[2] - z * t[1], v[1] + w * t[1] + z * t[0] - x * t[2], v[2] + w * t[2] + x * t[1] - y * t[0]]
    };
    let a = rot(p.rotation, [1.0, 0.0, 0.0]);
    let b = rot(jiggle.3, rot(q, [1.0, 0.0, 0.0]));
    assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5), "{a:?} {b:?}");
}

#[test]
fn markup_on_an_unknown_bone_is_dropped_and_named() {
    let mut m = markup();
    m.attachments[0].influences[0].bone = "tail_03".to_string();
    m.hitbox_sets[0].hitboxes[0].bone = "tail_03".to_string();
    let fitted = agent::fit(&m, &skel());
    assert_eq!(fitted.attachments.len(), ATTACHMENTS.len() - 1);
    assert_eq!(fitted.hitbox_sets[0].hitboxes.len(), HITBOXES.len() - 1);
    assert_eq!(fitted.dropped.len(), 2);
    assert!(fitted.dropped.iter().all(|d| d.contains("tail_03")), "{:?}", fitted.dropped);
}

#[test]
fn a_fold_never_ends_on_the_root_and_a_dropped_attachment_moves_nothing() {
    let mut m = markup();
    // re-parent jiggle_primary straight under root_motion: the agent root frame differs, so drop it
    let jiggle = m.bones.iter().position(|b| b.name == "jiggle_primary").unwrap();
    m.bones[jiggle].parent = Some(0);
    // and an attachment with no influences is reported, not skipped silently
    m.attachments[0].influences.clear();
    let fitted = agent::fit(&m, &skel());
    assert!(fitted.moved.is_empty(), "{:?}", fitted.moved);
    assert_eq!(fitted.attachments.len(), ATTACHMENTS.len() - 2);
    assert!(fitted.dropped.iter().any(|d| d.contains("primary") && d.contains("non-root")), "{:?}", fitted.dropped);
    assert!(fitted.dropped.iter().any(|d| d.contains("no influences")), "{:?}", fitted.dropped);
}

#[test]
fn angles_follow_the_source_qangle_convention() {
    assert_eq!(agent::quat_to_angles([0.0, 0.0, 0.0, 1.0]), [0.0, 0.0, 0.0]);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    // +90 about Z = yaw 90
    let a = agent::quat_to_angles([0.0, 0.0, h, h]);
    assert!((a[1] - 90.0).abs() < 1e-9 && a[0].abs() < 1e-9 && a[2].abs() < 1e-9, "{a:?}");
    // every agent rotation survives angles -> quaternion
    for row in ATTACHMENTS {
        let back = angles_to_quat(agent::quat_to_angles(row.4));
        assert!(same_rotation(back, row.4), "{}: {:?} -> {back:?}", row.0, row.4);
    }
}

#[test]
fn the_vmdl_carries_attachments_hitboxes_and_keeps_every_bone() {
    let plan = plan(Some(markup()));
    let lists = lists(&plan);
    let keep = list(&lists, "BoneMarkupList")["children"].as_array().unwrap();
    assert_eq!(keep.len(), 75, "one BoneMarkup per vnmskel bone");
    assert!(keep.iter().any(|b| b == &json!({"_class": "BoneMarkup", "target_bone": "wpnpivot", "do_not_discard": true})), "{keep:?}");
    assert!(list(&lists, "NmSkeletonList")["children"][0].get("do_not_discard_skeleton_bones").is_none());
    // the mixed-case vnmskel names come back after the SMD importer's lowercasing
    let mesh = &list(&lists, "RenderMeshList")["children"][0];
    let renames = mesh["children"].as_array().unwrap();
    assert_eq!(renames.len(), skel().names.iter().filter(|n| n.to_ascii_lowercase() != **n).count());
    assert!(renames.contains(&json!({"_class": "RenameBone", "original_bone_name": "hand_r", "new_bone_name": "hand_R"})), "{renames:?}");
    // the graph again under the identifier the pawn asks for
    let graphs = list(&lists, "AnimGraph2List")["children"].as_array().unwrap();
    assert_eq!(
        graphs,
        &vec![
            json!({"_class": "DefaultAnimGraph2", "filename": "animation/graphs/worldmodel/worldmodel.vnmgraph"}),
            json!({"_class": "AnimGraph2", "name": "worldmodel", "filename": "animation/graphs/worldmodel/worldmodel.vnmgraph"}),
        ]
    );
    let attachments = list(&lists, "AttachmentList")["children"].as_array().unwrap();
    assert_eq!(attachments.len(), ATTACHMENTS.len());
    let weapon = attachments.iter().find(|a| a["name"] == "weapon").unwrap();
    assert_eq!(
        weapon,
        &json!({"_class": "Attachment", "parent_bone": "wpn", "relative_origin": [0.0, 0.0, 0.0], "relative_angles": [0.0, 0.0, 0.0], "weight": 1.0, "name": "weapon", "ignore_rotation": false})
    );
    let hand = attachments.iter().find(|a| a["name"] == "weapon_hand_l").unwrap();
    assert_eq!(hand["parent_bone"], json!("hand_l"));
    close(&hand["relative_origin"], [2.6, 1.4, 0.0], 1e-4, "hand_l origin");
    let back = angles_to_quat(serde_json::from_value(hand["relative_angles"].clone()).unwrap());
    assert!(same_rotation(back, ATTACHMENTS.iter().find(|a| a.0 == "weapon_hand_l").unwrap().4) || {
        // 4-decimal rounding of the angles
        let q = ATTACHMENTS.iter().find(|a| a.0 == "weapon_hand_l").unwrap().4;
        back.iter().zip(q).map(|(x, y)| x * y).sum::<f64>().abs() > 1.0 - 1e-8
    });
    let center = attachments.iter().find(|a| a["name"] == "weapon_center").unwrap();
    assert_eq!(center["parent_bone"], json!(""));
    close(&center["relative_origin"], [0.0, 0.0, 40.0], 0.0, "weapon_center");
    // hitboxes
    let sets = list(&lists, "HitboxSetList")["children"].as_array().unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0]["name"], json!("cstrike"));
    let boxes = sets[0]["children"].as_array().unwrap();
    assert_eq!(boxes.len(), HITBOXES.len());
    let head = &boxes[0];
    assert_eq!(head["_class"], json!("HitboxCapsule"));
    assert_eq!(head["name"], json!("head_0"));
    assert_eq!(head["parent_bone"], json!("head_0"));
    assert_eq!(head["surface_property"], json!("playerflesh"));
    assert_eq!(head["group_id"], json!(1));
    assert_eq!(head["translation_only"], json!(false));
    assert_eq!(head["radius"], json!(4.3));
    close(&head["point0"], [-1.0, 1.8, 0.0], 1e-9, "point0");
    close(&head["point1"], [3.5, 0.2, 0.0], 1e-9, "point1");
    assert!(boxes.iter().all(|b| b["parent_bone"].as_str().unwrap() == b["parent_bone"].as_str().unwrap().to_ascii_lowercase()));
    // the plan reports it
    let (agent, fitted) = plan.markup.as_ref().unwrap();
    assert_eq!(agent, agent::DEFAULT_AGENT);
    assert_eq!(fitted.attachments.len(), ATTACHMENTS.len());
    // the written text names the classes the compiler reads
    let text = plan.vmdl.write();
    for needle in ["_class = \"Attachment\"", "_class = \"HitboxCapsule\"", "do_not_discard = true"] {
        assert!(text.contains(needle), "{needle}");
    }
}

#[test]
fn without_an_agent_the_lists_stay_empty_but_the_bones_are_kept() {
    let plan = plan(None);
    let lists = lists(&plan);
    assert_eq!(list(&lists, "AttachmentList")["children"], json!([]));
    let sets = list(&lists, "HitboxSetList")["children"].as_array().unwrap();
    assert_eq!(sets[0]["name"], json!("default"));
    assert_eq!(sets[0]["children"], json!([]));
    assert_eq!(list(&lists, "BoneMarkupList")["children"].as_array().unwrap().len(), 75);
    assert!(plan.markup.is_none());
}

#[test]
fn the_template_checks_attachment_and_hitbox_params() {
    let base = |extra: Value| {
        let mut p = json!({"meshFile": "models/x/x.smd"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        hammer_mcp::vmdl::template("skeletal", &p)
    };
    let e = base(json!({"attachments": [{"name": "a", "parent_bone": "b", "colour": 1}]})).unwrap_err();
    assert!(e.to_string().contains("colour"), "{e}");
    let e = base(json!({"attachments": [{"parent_bone": "b"}]})).unwrap_err();
    assert!(e.to_string().contains("name"), "{e}");
    let e = base(json!({"hitboxSets": [{"name": "s", "hitboxes": [{"shape": "cone"}]}]})).unwrap_err();
    assert!(e.to_string().contains("cone"), "{e}");
    let e = base(json!({"hitboxSets": [{"name": "s", "hitboxes": [{"shape": "sphere", "point0": [0, 0, 0]}]}]})).unwrap_err();
    assert!(e.to_string().contains("point0"), "{e}");
    let e = base(json!({"attachments": [{"name": "a", "relative_origin": [0, 0, 0], "relative_angles": [0, 0, 0]}]})).unwrap_err();
    assert!(e.to_string().contains("parent_bone"), "{e}");
    let e = base(json!({"hitboxSets": [{"name": "s", "hitboxes": [{"shape": "capsule", "name": "h", "parent_bone": "b", "radius": 1}]}]})).unwrap_err();
    assert!(e.to_string().contains("point0"), "{e}");
    let e = hammer_mcp::vmdl::template("static_prop", &json!({"meshFile": "models/x/x.smd", "attachments": []})).unwrap_err();
    assert!(e.to_string().contains("skeletal"), "{e}");
    let e = base(json!({"renameBones": [{"from": "a", "to": "a"}]})).unwrap_err();
    assert!(e.to_string().contains("equal"), "{e}");
    let e = base(json!({"renameBones": [{"from": "a"}]})).unwrap_err();
    assert!(e.to_string().contains("\"to\""), "{e}");
    let e = hammer_mcp::vmdl::template("static_prop", &json!({"meshFile": "models/x/x.smd", "renameBones": [{"from": "a", "to": "A"}]})).unwrap_err();
    assert!(e.to_string().contains("skeletal"), "{e}");
    let e = base(json!({"keepBones": "wpn"})).unwrap_err();
    assert!(e.to_string().contains("keepBones"), "{e}");
    let e = hammer_mcp::vmdl::template("skeletal", &json!({"meshFile": "models/x/x.smd", "animGraph2Identifiers": ["worldmodel"]})).unwrap_err();
    assert!(e.to_string().contains("needs params.animGraph2"), "{e}");
    // a multi-influence attachment and the three shapes
    let m = base(json!({
        "attachments": [{"name": "a", "parent_bone": "b", "relative_origin": [1, 2, 3], "relative_angles": [0, 0, 0], "weight": 0.5,
                          "influences": [{"parent_bone": "c", "relative_origin": [0, 0, 0], "relative_angles": [0, 0, 0], "weight": 0.5}]}],
        "hitboxSets": [{"name": "s", "hitboxes": [
            {"shape": "sphere", "name": "s1", "parent_bone": "b", "center": [0, 0, 0], "radius": 2},
            {"shape": "box", "name": "b1", "parent_bone": "b", "hitbox_mins": [0, 0, 0], "hitbox_maxs": [1, 1, 1]}]}]
    }))
    .expect("valid");
    let text = m.write();
    for needle in ["_class = \"AttachmentInfluence\"", "_class = \"HitboxSphere\"", "_class = \"Hitbox\"", "hitbox_maxs"] {
        assert!(text.contains(needle), "{needle} in {text}");
    }
    assert!(!text.contains("shape ="), "shape is a selector, not a key");
}

// ------------------------------------------------------------------------------ the real game

/// The installed pak (`HAMMER_MCP_GAME_PAK` can point at another one), as in `model_import_player.rs`.
fn pak() -> hammer_mcp::vpk::VpkDir {
    let pak = std::env::var("HAMMER_MCP_GAME_PAK")
        .unwrap_or_else(|_| {
            let root = std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
            format!("{root}/workshop/run/app/game/csgo/pak01_dir.vpk")
        });
    hammer_mcp::vpk::VpkDir::open(Path::new(&pak)).expect("open pak")
}

#[test]
#[ignore = "needs the CS2 install"]
fn the_installed_default_agent_matches_the_fixture() {
    let dir = pak();
    let bytes = dir.read("agents/models/ctm_sas/ctm_sas.vmdl_c").expect("ctm_sas in pak01");
    let real = agent::from_resource_bytes(&bytes).expect("decodes");
    let fixture = markup();
    assert_eq!(real.attachments, fixture.attachments);
    assert_eq!(real.hitbox_sets, fixture.hitbox_sets);
    // and the fit on the real skeleton drops nothing
    let skel_bytes = dir.read(skeleton::SKELETON_COMPILED).expect("vnmskel in pak01");
    let fitted = agent::fit(&real, &skeleton::from_resource_bytes(&skel_bytes).unwrap());
    assert!(fitted.dropped.is_empty(), "{:?}", fitted.dropped);
}

#[test]
#[ignore = "needs the CS2 install"]
fn every_installed_agent_has_the_same_hitboxes() {
    let dir = pak();
    let agents: Vec<String> = dir
        .paths()
        .filter(|p| p.starts_with("agents/models/") && p.ends_with(".vmdl_c") && !p.starts_with("agents/models/shared/"))
        .map(str::to_string)
        .collect();
    assert!(agents.len() > 50, "{}", agents.len());
    let reference = markup().hitbox_sets;
    for path in agents {
        let m = agent::from_resource_bytes(&dir.read(&path).unwrap()).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(m.hitbox_sets, reference, "{path}");
        assert!(m.attachments.iter().any(|a| a.name == "weapon"), "{path}");
    }
}

