//! `model_import kind:"player"` (I6): the retarget onto the CS2 player skeleton against a
//! synthetic `worldmodel.vnmskel_c` (the real file's 75 bones and poses, `tests/common/
//! vnmskel_data.rs`) and a procedural Mixamo-named humanoid. Expected numbers are worked out here
//! from the vnmskel data and the fixture's offsets. No game needed.
#![allow(clippy::needless_range_loop, clippy::result_large_err)]

use hammer_mcp::import::mat::{apply3, Mat3};
use hammer_mcp::import::retarget::bake::quat_mat;
use hammer_mcp::import::retarget::mapping::RigHint;
use hammer_mcp::import::retarget::skeleton::{self, NmSkeleton};
use hammer_mcp::import::retarget::PlayerOptions;
use hammer_mcp::import::skeleton::{euler_of, matrix_of_euler};
use hammer_mcp::import::{self, ImportError, ImportOptions, ImportPlan, Kind};
use std::path::Path;

mod common;
use common::gltf_fixture::{Fixture, RigFixture};
use common::humanoid::{self, Humanoid, Naming, Opts};

fn skel() -> NmSkeleton {
    skeleton::from_resource_bytes(&common::vnmskel_c()).expect("fixture skeleton")
}

fn player(map: &[(&str, &str)], rig: RigHint) -> PlayerOptions {
    PlayerOptions {
        skeleton: skel(),
        bone_map: map.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        rig,
        markup: None,
    }
}

fn opts(player: PlayerOptions) -> ImportOptions {
    ImportOptions {
        name: "hero".into(),
        kind: Kind::Player,
        scale: None,
        up: None,
        generate_materials: true,
        existing: None,
        physics: None,
        player: Some(player),
    }
}

fn plan_with(h: &RigFixture, p: PlayerOptions) -> Result<ImportPlan, ImportError> {
    import::plan(Path::new("hero.glb"), h.glb(), &opts(p))
}

fn plan_ok(h: &Humanoid) -> ImportPlan {
    plan_with(&h.fixture, player(&[], RigHint::Auto)).expect("plan")
}

fn text(plan: &ImportPlan, path: &str) -> String {
    let f = plan.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("no {path}"));
    String::from_utf8(f.bytes.clone()).unwrap()
}

// ------------------------------------------------------------------------------ SMD parsing

struct Vert {
    p: [f64; 3],
    n: [f64; 3],
    links: Vec<(usize, f64)>,
}

struct Smd {
    nodes: Vec<(String, i64)>,
    pose: Vec<([f64; 3], [f64; 3])>,
    tris: Vec<[Vert; 3]>,
}

/// The compiler's Rz(+90°) turn of SMD content (SPIKE-I0); the writer pre-turns by Rz(−90°), so
/// the parser applies this to read the model as compiled (vnmskel model space).
fn compiled(v: [f64; 3]) -> [f64; 3] {
    [-v[1], v[0], v[2]]
}

/// Parse a reference SMD and undo the writer's pre-turn (root bones, vertices, normals).
fn parse(text: &str) -> Smd {
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("version 1"));
    assert_eq!(lines.next(), Some("nodes"));
    let mut nodes = Vec::new();
    for l in lines.by_ref() {
        if l == "end" {
            break;
        }
        let (id, rest) = l.split_once(' ').unwrap();
        assert_eq!(id.parse::<usize>().unwrap(), nodes.len());
        let (name, parent) = rest.rsplit_once(' ').unwrap();
        nodes.push((name.trim_matches('"').to_string(), parent.parse().unwrap()));
    }
    assert_eq!(lines.next(), Some("skeleton"));
    assert_eq!(lines.next(), Some("time 0"));
    let mut pose = Vec::new();
    for l in lines.by_ref() {
        if l == "end" {
            break;
        }
        let v: Vec<f64> = l.split_whitespace().map(|x| x.parse().unwrap()).collect();
        assert_eq!(v[0] as usize, pose.len());
        let (t, e) = ([v[1], v[2], v[3]], [v[4], v[5], v[6]]);
        if nodes[pose.len()].1 < 0 {
            let r = matrix_of_euler(e);
            let turn: Mat3 = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
            let mut tr = [[0.0; 3]; 3];
            for (i, row) in tr.iter_mut().enumerate() {
                for (j, cell) in row.iter_mut().enumerate() {
                    *cell = (0..3).map(|k| turn[i][k] * r[k][j]).sum();
                }
            }
            pose.push((compiled(t), euler_of(&tr)));
        } else {
            pose.push((t, e));
        }
    }
    assert_eq!(lines.next(), Some("triangles"));
    let mut tris = Vec::new();
    while let Some(material) = lines.next() {
        if material == "end" {
            break;
        }
        let mut vs = Vec::new();
        for _ in 0..3 {
            let f: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
            let n: usize = f[9].parse().unwrap();
            assert_eq!(f.len(), 10 + 2 * n);
            let num = |i: usize| f[i].parse::<f64>().unwrap();
            vs.push(Vert {
                p: compiled([num(1), num(2), num(3)]),
                n: compiled([num(4), num(5), num(6)]),
                links: (0..n).map(|k| (f[10 + 2 * k].parse().unwrap(), num(11 + 2 * k))).collect(),
            });
        }
        tris.push(<[Vert; 3]>::try_from(vs).ok().unwrap());
    }
    Smd { nodes, pose, tris }
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}
fn unit(a: [f64; 3]) -> [f64; 3] {
    let l = norm(a);
    [a[0] / l, a[1] / l, a[2] / l]
}
fn close(a: [f64; 3], b: [f64; 3], tol: f64, what: &str) {
    assert!(norm(sub(a, b)) <= tol, "{what}: {a:?} vs {b:?}");
}

fn model_t(s: &NmSkeleton, name: &str) -> [f64; 3] {
    s.model[s.index(name).unwrap_or_else(|| panic!("no {name}"))].t
}

/// The three probe/nose vertices of the plan's SMD: the triangle list ends with the probe, then the nose.
fn tail(smd: &Smd) -> (&[Vert; 3], &[Vert; 3]) {
    let n = smd.tris.len();
    (&smd.tris[n - 2], &smd.tris[n - 1])
}

fn mapped<'a>(plan: &'a ImportPlan, source: &str) -> Option<(&'a str, &'a str)> {
    let r = plan.player.as_ref().unwrap();
    r.mapped.iter().find(|m| m.source == source).map(|m| (m.cs2.as_str(), m.how.as_str()))
}

// ------------------------------------------------------------------------------ skeleton

#[test]
fn the_skeleton_fixture_round_trips_the_real_layout() {
    let s = skel();
    assert_eq!(s.names.len(), 75);
    assert_eq!(s.names[1], "pelvis");
    assert_eq!(s.parents[1], Some(0));
    assert_eq!(s.parents[0], None);
    assert_eq!(s.id, "animation/skeletons/characters/worldmodel.vnmskel");
    // model space = parent chain of the local poses (the file's own numbers agree to float precision)
    let compose = |i: usize| -> ([f64; 3], Mat3) {
        let mut chain = vec![i];
        while let Some(p) = s.parents[*chain.last().unwrap()] {
            chain.push(p);
        }
        let (mut t, mut r): ([f64; 3], Mat3) = ([0.0; 3], [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        for &b in chain.iter().rev() {
            let l = &s.local[b];
            let moved = apply3(&r, l.t);
            t = [t[0] + moved[0], t[1] + moved[1], t[2] + moved[2]];
            r = hammer_mcp::import::mat::mul3(&r, &quat_mat(l.q));
        }
        (t, r)
    };
    for i in 0..75 {
        close(compose(i).0, s.model[i].t, 1e-3, &s.names[i]);
    }
}

#[test]
fn a_malformed_skeleton_is_a_typed_error() {
    let mut bytes = common::vnmskel_c();
    bytes.truncate(40);
    assert!(skeleton::from_resource_bytes(&bytes).is_err());
}

/// The real file, when the game is installed (`HAMMER_MCP_GAME_PAK` can point at another pak).
#[test]
#[ignore = "needs the CS2 install"]
fn the_installed_skeleton_matches_the_fixture() {
    let pak = std::env::var("HAMMER_MCP_GAME_PAK")
        .unwrap_or_else(|_| {
            let root = std::env::var("CS2_MCP_PROJECT_ROOT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/..").to_string());
            format!("{root}/workshop/run/app/game/csgo/pak01_dir.vpk")
        });
    let vpk = hammer_mcp::vpk::VpkDir::open(Path::new(&pak)).expect("open pak");
    let bytes = vpk.read(skeleton::SKELETON_COMPILED).expect("worldmodel.vnmskel_c in the pak");
    let real = skeleton::from_resource_bytes(&bytes).expect("decode");
    let fixture = skel();
    assert_eq!(real.names, fixture.names);
    assert_eq!(real.parents, fixture.parents);
    for i in 0..real.names.len() {
        close(real.local[i].t, fixture.local[i].t, 1e-3, &real.names[i]);
        close(real.model[i].t, fixture.model[i].t, 1e-3, &real.names[i]);
    }
}

// ------------------------------------------------------------------------------ mapping

const REQUIRED_L: [(&str, &str); 7] = [
    ("Shoulder", "clavicle"),
    ("Arm", "arm_upper"),
    ("ForeArm", "arm_lower"),
    ("Hand", "hand"),
    ("UpLeg", "leg_upper"),
    ("Leg", "leg_lower"),
    ("Foot", "ankle"),
];

/// How many of the 18 required roles are filled (spine counts once).
fn required_covered(plan: &ImportPlan) -> usize {
    let r = plan.player.as_ref().unwrap();
    let have = |n: &str| r.mapped.iter().any(|m| m.cs2 == n);
    let mut c = ["pelvis", "neck_0", "head_0"].iter().filter(|n| have(n)).count();
    c += (0..4).any(|i| have(&format!("spine_{i}"))) as usize;
    for side in ["L", "R"] {
        c += REQUIRED_L.iter().filter(|(_, b)| have(&format!("{b}_{side}"))).count();
    }
    c
}

#[test]
fn the_mixamo_humanoid_maps_the_whole_required_set_by_table() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    assert_eq!(required_covered(&plan), 18);
    let r = plan.player.as_ref().unwrap();
    assert_eq!(r.rig, "mixamo");
    assert_eq!(r.confidence, 1.0);
    assert_eq!(r.bones, 75);
    assert!(r.mapped.iter().all(|m| m.how == "table:mixamo"), "{:?}", r.mapped);
    // names: the left side is the character's left
    assert_eq!(mapped(&plan, "mixamorig:LeftForeArm"), Some(("arm_lower_L", "table:mixamo")));
    assert_eq!(mapped(&plan, "mixamorig:RightUpLeg"), Some(("leg_upper_R", "table:mixamo")));
    // three spine joints spread over spine_0..3 by even spacing: 0, 2, 3
    assert_eq!(mapped(&plan, "mixamorig:Spine").unwrap().0, "spine_0");
    assert_eq!(mapped(&plan, "mixamorig:Spine1").unwrap().0, "spine_2");
    assert_eq!(mapped(&plan, "mixamorig:Spine2").unwrap().0, "spine_3");
    // fingers of the left hand (3 joints -> 0,1,2; the metacarpal slot stays open)
    assert_eq!(mapped(&plan, "mixamorig:LeftHandIndex1").unwrap().0, "finger_index_0_L");
    assert_eq!(mapped(&plan, "mixamorig:LeftHandIndex3").unwrap().0, "finger_index_2_L");
    assert_eq!(mapped(&plan, "mixamorig:LeftHandThumb1").unwrap().0, "finger_thumb_0_L");
    assert_eq!(mapped(&plan, "mixamorig:LeftToeBase").unwrap().0, "ball_L");
}

#[test]
fn unmapped_joints_hand_their_weights_to_the_nearest_mapped_ancestor() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let r = plan.player.as_ref().unwrap();
    let moved = |bone: &str| r.unmapped.iter().find(|u| u.bone == bone).map(|u| u.weights_moved_to.as_str());
    // the eyes have no vertices; Toe_End and HeadTop_End carry the tips of their boxes' parents only
    assert!(r.unmapped.iter().all(|u| moved(&u.bone).is_some()));
    assert!(r.unweighted >= 2, "{r:?}");
}

#[test]
fn a_weighted_unmapped_joint_goes_to_its_mapped_ancestor() {
    // bind the nose to the (unmapped) left eye instead of the head
    let mut h = humanoid::build(&Opts::default());
    let eye = h.index("LeftEye") as u16;
    let joints = h.fixture.meshes[0].joints.as_mut().unwrap();
    for j in &mut joints[h.nose..h.nose + 3] {
        j[0] = eye;
    }
    let plan = plan_ok(&h);
    let r = plan.player.as_ref().unwrap();
    let u = r.unmapped.iter().find(|u| u.bone == "mixamorig:LeftEye").expect("the eye now carries weight");
    assert_eq!(u.weights_moved_to, "head_0");
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let (_, nose) = tail(&smd);
    let head = skel().index("head_0").unwrap();
    assert!(nose.iter().all(|v| v.links == vec![(head, 1.0)]));
}

#[test]
fn renamed_bones_are_found_by_structure() {
    let h = humanoid::build(&Opts { naming: Naming::Generic, ..Opts::default() });
    let plan = plan_ok(&h);
    let r = plan.player.as_ref().unwrap();
    assert_eq!(r.rig, "none");
    let covered = required_covered(&plan);
    assert!(covered * 10 >= 18 * 9, "structural covered {covered}/18");
    assert!(r.confidence < 1.0 && r.confidence >= 0.7, "{}", r.confidence);
    assert!(r.mapped.iter().all(|m| m.how == "structural"));
    // the same bones as the Mixamo names would give: b05 is the head (index 5 in bone order)
    let idx = |short: &str| humanoid::build(&Opts::default()).index(short);
    assert_eq!(mapped(&plan, &format!("b{:02}", idx("Head"))).unwrap().0, "head_0");
    assert_eq!(mapped(&plan, &format!("b{:02}", idx("LeftForeArm"))).unwrap().0, "arm_lower_L");
    assert_eq!(mapped(&plan, &format!("b{:02}", idx("RightFoot"))).unwrap().0, "ankle_R");
    assert_eq!(mapped(&plan, &format!("b{:02}", idx("Hips"))).unwrap().0, "pelvis");
}

#[test]
fn bone_map_overrides_the_tables() {
    let h = humanoid::build(&Opts::default());
    // send Spine1 to spine_1 (the table would use spine_2) and HeadTop_End to head_0 (the table: Head)
    let p = player(&[("mixamorig:Spine1", "spine_1"), ("mixamorig:HeadTop_End", "head_0")], RigHint::Auto);
    let plan = plan_with(&h.fixture, p).expect("plan");
    assert_eq!(mapped(&plan, "mixamorig:Spine1"), Some(("spine_1", "explicit")));
    assert_eq!(mapped(&plan, "mixamorig:HeadTop_End"), Some(("head_0", "explicit")));
    // the table's own claim on head_0 is gone
    assert_eq!(mapped(&plan, "mixamorig:Head"), None);
    assert_eq!(required_covered(&plan), 18);
    // case-insensitive on both sides
    let p = player(&[("MIXAMORIG:SPINE1", "SPINE_1")], RigHint::Auto);
    let plan = plan_with(&h.fixture, p).expect("plan");
    assert_eq!(mapped(&plan, "mixamorig:Spine1"), Some(("spine_1", "explicit")));
}

fn invalid(h: &Humanoid, map: &[(&str, &str)]) -> (String, Vec<String>) {
    match plan_with(&h.fixture, player(map, RigHint::Auto)) {
        Err(ImportError::BoneMap { detail, near }) => (detail, near),
        other => panic!("expected BoneMap, got {:?}", other.map(|p| p.model)),
    }
}

#[test]
fn bone_map_mistakes_are_refused_with_near_misses() {
    let h = humanoid::build(&Opts::default());
    let (detail, near) = invalid(&h, &[("mixamorig:LeftForeAr", "arm_lower_L")]);
    assert!(detail.contains("not in the file's skin"), "{detail}");
    assert!(near.contains(&"mixamorig:LeftForeArm".to_string()), "{near:?}");
    let (detail, near) = invalid(&h, &[("mixamorig:Head", "head_1")]);
    assert!(detail.contains("not a CS2 player bone"), "{detail}");
    assert!(near.contains(&"head_0".to_string()), "{near:?}");
    let (detail, _) = invalid(&h, &[("mixamorig:Head", "wpnHand_L")]);
    assert!(detail.contains("never carries weights"), "{detail}");
    let (detail, _) = invalid(&h, &[("mixamorig:Head", "camera")]);
    assert!(detail.contains("never carries weights"), "{detail}");
    let (detail, _) = invalid(&h, &[("mixamorig:Head", "spine_1"), ("mixamorig:Neck", "spine_1")]);
    assert!(detail.contains("takes one source joint"), "{detail}");
}

#[test]
fn a_bone_map_that_runs_a_chain_up_the_hierarchy_is_refused() {
    let h = humanoid::build(&Opts::default());
    // Spine on spine_3 leaves the table's Spine1 (below it) earlier in the torso chain
    let (detail, _) = invalid(&h, &[("mixamorig:Spine", "spine_3")]);
    assert!(detail.contains("is above") && detail.contains("mixamorig:Spine"), "{detail}");
}

#[test]
fn missing_required_bones_are_named() {
    let h = humanoid::build(&Opts { omit: vec!["LeftForeArm"], ..Opts::default() });
    match plan_with(&h.fixture, player(&[], RigHint::Auto)) {
        Err(ImportError::BoneMapIncomplete { unmapped, required }) => {
            assert_eq!(unmapped, vec!["arm_lower_L".to_string()]);
            assert_eq!(required.len(), 18);
            assert!(required.contains(&"spine_*".to_string()));
        }
        other => panic!("expected BoneMapIncomplete, got {:?}", other.map(|p| p.model)),
    }
    // ... and a boneMap fixes it
    let p = player(&[("mixamorig:LeftHand", "arm_lower_L"), ("mixamorig:LeftHandIndex1", "hand_L")], RigHint::Auto);
    assert!(plan_with(&h.fixture, p).is_ok());
    // a two-bone rig has almost nothing
    match plan_with(&RigFixture::tube(), player(&[], RigHint::Auto)) {
        Err(ImportError::BoneMapIncomplete { unmapped, .. }) => {
            assert!(unmapped.contains(&"pelvis".to_string()) && unmapped.contains(&"head_0".to_string()), "{unmapped:?}");
        }
        other => panic!("expected BoneMapIncomplete, got {:?}", other.map(|p| p.model)),
    }
}

#[test]
fn a_static_file_has_no_skin() {
    let bytes = Fixture::cube().glb();
    match import::plan(Path::new("cube.glb"), bytes, &opts(player(&[], RigHint::Auto))) {
        Err(ImportError::NoSkin) => {}
        other => panic!("expected NoSkin, got {:?}", other.map(|p| p.model)),
    }
}

// ------------------------------------------------------------------------------ output

#[test]
fn the_smd_skeleton_is_the_vnmskel_parent_space_pose_verbatim() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let s = skel();
    assert_eq!(smd.nodes.len(), 75);
    for i in 0..75 {
        assert_eq!(smd.nodes[i].0, s.names[i], "bone {i}");
        assert_eq!(smd.nodes[i].1, s.parents[i].map_or(-1, |p| p as i64), "parent of {}", s.names[i]);
        close(smd.pose[i].0, s.local[i].t, 1e-5, &format!("translation of {}", s.names[i]));
        // the Euler round trip: Rz Ry Rx of the written angles is the quaternion's matrix
        let got = matrix_of_euler(smd.pose[i].1);
        let want = quat_mat(s.local[i].q);
        for r in 0..3 {
            for c in 0..3 {
                assert!((got[r][c] - want[r][c]).abs() < 2e-5, "rotation of {}: {got:?} vs {want:?}", s.names[i]);
            }
        }
    }
}

#[test]
fn no_weight_lands_on_the_game_driven_bones() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let s = skel();
    let mut used = std::collections::BTreeSet::new();
    for tri in &smd.tris {
        for v in tri {
            assert!(v.links.len() <= 4);
            assert!((v.links.iter().map(|l| l.1).sum::<f64>() - 1.0).abs() < 1e-4);
            for (b, _) in &v.links {
                assert!(!NmSkeleton::is_weightless(&s.names[*b]), "weight on {}", s.names[*b]);
                used.insert(*b);
            }
        }
    }
    assert!(used.len() > 15);
}

#[test]
fn the_vmdl_carries_the_two_nm_nodes_and_nothing_else_of_the_animation() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let vmdl = text(&plan, "models/hero/hero.vmdl");
    for needle in [
        "_class = \"NmSkeletonList\"",
        "_class = \"NmSkeletonReference\"",
        "filename = \"animation/skeletons/characters/worldmodel.vnmskel\"",
        "_class = \"AnimGraph2List\"",
        "_class = \"DefaultAnimGraph2\"",
        "filename = \"animation/graphs/worldmodel/worldmodel.vnmgraph\"",
    ] {
        assert!(vmdl.contains(needle), "{needle} missing in\n{vmdl}");
    }
    assert!(!vmdl.contains("AnimFile"), "{vmdl}");
    assert!(vmdl.contains("filename = \"models/hero/hero.smd\""));
    assert!(vmdl.contains("import_scale = 1.0"));
    assert!(!vmdl.contains("PhysicsHullFile") && !vmdl.contains("PhysicsMeshFile"), "no physics by default");
    assert_eq!(plan.physics.as_str(), "none");
    // the order of the spike's spike_agent.vmdl
    let at = |n: &str| vmdl.find(n).unwrap();
    assert!(at("HitboxSetList") < at("NmSkeletonList") && at("NmSkeletonList") < at("AnimGraph2List") && at("AnimGraph2List") < at("AttachmentList"));
}

#[test]
fn source_animations_are_dropped_with_a_note() {
    let mut h = humanoid::build(&Opts::default());
    h.fixture.anims.push(common::gltf_fixture::RigAnim {
        name: Some("Walk".into()),
        channels: vec![common::gltf_fixture::RigChannel {
            node: 1,
            path: "translation",
            interpolation: "LINEAR",
            times: vec![0.0, 1.0],
            values: vec![0.0, 1.0, 0.0, 0.0, 1.1, 0.0],
        }],
    });
    let plan = plan_ok(&h);
    assert!(plan.dropped.iter().any(|d| d.contains("AnimGraph2")), "{:?}", plan.dropped);
    assert!(plan.rig.is_none());
    assert!(!plan.files.iter().any(|f| f.role == "animation"));
}

// ------------------------------------------------------------------------------ facing & math

#[test]
fn the_cs2_axes_are_derived_from_the_skeleton_data() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let r = plan.player.as_ref().unwrap();
    let [up, left, fwd] = r.cs2_axes;
    close(left, [0.0, 1.0, 0.0], 1e-6, "left is +Y");
    assert!(dot(up, [0.0, 0.0, 1.0]) > 0.997, "up {up:?}");
    assert!(dot(fwd, [1.0, 0.0, 0.0]) > 0.997, "forward {fwd:?}");
    close(cross(left, up), fwd, 1e-9, "right-handed: left x up = forward");
    // the glTF source: +Y up, +X left, +Z front
    let [up, left, fwd] = r.src_axes;
    close(up, [0.0, 1.0, 0.0], 1e-6, "source up");
    close(left, [1.0, 0.0, 0.0], 1e-6, "source left");
    close(fwd, [0.0, 0.0, 1.0], 1e-6, "source forward");
}

/// The head-to-pelvis fit of the fixture: (0.1 + 0.15 + 0.15 + 0.15 + 0.1) m against the vnmskel's.
fn fit() -> f64 {
    let s = skel();
    norm(sub(model_t(&s, "head_0"), model_t(&s, "pelvis"))) / 0.65
}

#[test]
fn the_scale_is_the_pelvis_to_head_fit_unless_given() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let r = plan.player.as_ref().unwrap();
    assert!((r.scale - fit()).abs() < 1e-4, "{} vs {}", r.scale, fit());
    assert!(!r.scale_from_arg);
    assert!((plan.scale - r.scale).abs() < 1e-12);
    let mut o = opts(player(&[], RigHint::Auto));
    o.scale = Some(39.37);
    let plan = import::plan(Path::new("hero.glb"), h.fixture.glb(), &o).unwrap();
    assert_eq!(plan.player.as_ref().unwrap().scale, 39.37);
    assert!(plan.player.as_ref().unwrap().scale_from_arg);
}

fn check_forearm(arm_down: f64) {
    let h = humanoid::build(&Opts { arm_down_degrees: arm_down, ..Opts::default() });
    let plan = plan_ok(&h);
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let s = skel();
    let (probe, _) = tail(&smd);
    // the probe is rigid on the left forearm -> arm_lower_L
    let lower = s.index("arm_lower_L").unwrap();
    for v in probe {
        assert_eq!(v.links, vec![(lower, 1.0)]);
    }
    let (a, b) = (model_t(&s, "arm_lower_L"), model_t(&s, "hand_L"));
    let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0, (a[2] + b[2]) / 2.0];
    let dir = unit(sub(b, a));
    // vertex 1 sat on the forearm's midpoint: the stretch puts it on the CS2 forearm's midpoint
    close(probe[1].p, mid, 2e-4, &format!("forearm midpoint ({arm_down} deg)"));
    // vertices 0 and 2 sat 2 cm off the axis (in the arm plane / along the front): k * 2 cm off the CS2 axis
    let k = fit();
    for (i, v) in [(0, &probe[0]), (2, &probe[2])] {
        let off = sub(v.p, mid);
        let along = dot(off, dir);
        let perp = norm(sub(off, [dir[0] * along, dir[1] * along, dir[2] * along]));
        assert!((perp - 0.02 * k).abs() < 2e-4, "probe {i}: {perp} vs {}", 0.02 * k);
        assert!(along.abs() < 2e-4, "probe {i} moved along the bone by {along}");
    }
    // the roll: the front-offset vertex lands along the front direction (the CS2 forward axis,
    // projected perpendicular to the CS2 forearm), not along the up/down one
    let [_, _, fwd] = plan.player.as_ref().unwrap().cs2_axes;
    let want = unit(sub(fwd, [dir[0] * dot(fwd, dir), dir[1] * dot(fwd, dir), dir[2] * dot(fwd, dir)]));
    close(unit(sub(probe[2].p, mid)), want, 1e-3, &format!("roll ({arm_down} deg)"));
}

#[test]
fn a_forearm_vertex_follows_the_cs2_forearm_for_a_t_pose_source() {
    check_forearm(0.0);
}

#[test]
fn a_forearm_vertex_follows_the_cs2_forearm_for_an_a_pose_source() {
    check_forearm(45.0);
}

#[test]
fn only_the_segment_itself_stretches() {
    // the Hips box runs 3 cm below the hips to 3 cm above Spine (10 cm up), all on the pelvis;
    // the CS2 pelvis -> spine_0 is under an inch, so the segment shrinks to it, but the part below
    // the joint stays rigid and the part past Spine is carried along, not squashed
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let s = skel();
    let pelvis = s.index("pelvis").unwrap();
    let (p0, p1) = (model_t(&s, "pelvis"), model_t(&s, "spine_0"));
    let dc = unit(sub(p1, p0));
    let along: Vec<f64> = smd
        .tris
        .iter()
        .flatten()
        .filter(|v| v.links == vec![(pelvis, 1.0)])
        .map(|v| dot(sub(v.p, p0), dc))
        .collect();
    assert!(!along.is_empty());
    let (lo, hi) = along.iter().fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
    let k = fit();
    assert!((lo + 0.03 * k).abs() < 1e-3, "below the hips: {lo} vs {}", -0.03 * k);
    assert!((hi - norm(sub(p1, p0)) - 0.03 * k).abs() < 1e-3, "past Spine: {hi} vs {}", norm(sub(p1, p0)) + 0.03 * k);
}

#[test]
fn the_nose_ends_on_the_forward_side() {
    for arm in [0.0, 45.0] {
        let h = humanoid::build(&Opts { arm_down_degrees: arm, ..Opts::default() });
        let plan = plan_ok(&h);
        let smd = parse(&text(&plan, "models/hero/hero.smd"));
        let (_, nose) = tail(&smd);
        let head = model_t(&skel(), "head_0");
        let fwd = plan.player.as_ref().unwrap().cs2_axes[2];
        let off = sub(nose[0].p, head);
        // 12 cm in front of the head joint, scaled
        assert!((norm(off) - 0.12 * fit()).abs() < 1e-3 * fit(), "{}", norm(off));
        assert!(dot(unit(off), fwd) > 0.97, "nose offset {off:?} vs forward {fwd:?}");
        assert!(dot(unit(off), [1.0, 0.0, 0.0]) > 0.97, "not along the vnmskel +X: {off:?}");
        // normals follow: the nose normal faces +Z in glTF, so forward here
        assert!(dot(unit(nose[0].n), fwd) > 0.9, "{:?}", nose[0].n);
    }
}

#[test]
fn the_mesh_stands_in_vnmskel_model_space() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let smd = parse(&text(&plan, "models/hero/hero.smd"));
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for tri in &smd.tris {
        for v in tri {
            for k in 0..3 {
                lo[k] = lo[k].min(v.p[k]);
                hi[k] = hi[k].max(v.p[k]);
            }
        }
    }
    // feet near the ground, head near the head bone, the pelvis in the middle of the width
    assert!(lo[2] > -6.0 && lo[2] < 6.0, "{lo:?}");
    assert!(hi[2] > 65.0 && hi[2] < 90.0, "{hi:?}");
    assert!(lo[1] < -15.0 && hi[1] > 15.0, "arms reach out sideways: {lo:?} {hi:?}");
}

#[test]
fn materials_and_the_result_blocks_come_with_the_plan() {
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    let (skeleton, retarget) = plan.player.as_ref().unwrap().json();
    assert_eq!(skeleton["bones"], 75);
    assert_eq!(skeleton["confidence"], 1.0);
    assert!(skeleton["mapped"].as_array().unwrap().len() >= 18);
    assert_eq!(retarget["rig"], "mixamo");
    assert_eq!(retarget["scaleFrom"], "pelvis-to-head fit");
    assert!(retarget["facing"]["cs2"]["forward"].is_array());
    assert!(plan.files.iter().any(|f| f.role == "model"));
}

#[test]
fn player_needs_a_gltf() {
    let o = opts(player(&[], RigHint::Auto));
    match import::plan(Path::new("hero.fbx"), vec![0u8; 8], &o) {
        Err(ImportError::Invalid(d)) => assert!(d.contains("glTF"), "{d}"),
        other => panic!("{:?}", other.map(|p| p.model)),
    }
}

// ------------------------------------------------------------------------------ name tables

mod tables {
    use super::*;
    use hammer_mcp::import::gltf_rig::{Joint, Rig};
    use hammer_mcp::import::mat::IDENTITY;
    use hammer_mcp::import::retarget::mapping::{self, canon, Side};

    fn rig_of(names: &[String]) -> Rig {
        Rig {
            joints: names.iter().map(|n| Joint { name: n.clone(), parent: None, bind: IDENTITY, synthetic: false }).collect(),
            clips: vec![],
        }
    }

    fn map(names: &[String], hint: RigHint) -> (Vec<(String, String)>, Option<&'static str>) {
        let s = skel();
        let m = mapping::build(&rig_of(names), &s, &[], hint).expect("the table covers the required set");
        let pairs = m.pairs.iter().map(|p| (names[p.src].clone(), s.names[p.cs2].clone())).collect();
        (pairs, m.rig)
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn sided(templates: &[&str], l: &str, r: &str) -> Vec<String> {
        templates.iter().flat_map(|t| [t.replace('#', l), t.replace('#', r)]).collect()
    }

    fn has(pairs: &[(String, String)], src: &str, cs2: &str) -> bool {
        pairs.iter().any(|(a, b)| a == src && b == cs2)
    }

    #[test]
    fn canonical_names() {
        assert_eq!(canon("mixamorig:LeftUpLeg", false), ("upleg".into(), Some(Side::L)));
        assert_eq!(canon("mixamorig1:RightHandIndex2", false), ("handindex2".into(), Some(Side::R)));
        assert_eq!(canon("Armature|Hips", false), ("hips".into(), None));
        assert_eq!(canon("DEF-upper_arm.L", true), ("upperarm".into(), Some(Side::L)));
        assert_eq!(canon("DEF-upper_arm.L.001", true), ("upperarml001".into(), None));
        assert_eq!(canon("DEF-spine.001", true), ("spine001".into(), None));
        assert_eq!(canon("upperarm_r", false), ("upperarm".into(), Some(Side::R)));
        assert_eq!(canon("index_01_l", false), ("index01".into(), Some(Side::L)));
        assert_eq!(canon("leftUpperArm", false), ("upperarm".into(), Some(Side::L)));
        assert_eq!(canon("Left_Hand", false), ("hand".into(), Some(Side::L)));
        assert_eq!(canon("head", false), ("head".into(), None));
    }

    #[test]
    fn rigify() {
        let mut names = strings(&["DEF-spine", "DEF-spine.001", "DEF-spine.002", "DEF-spine.003", "DEF-spine.004", "DEF-spine.005", "DEF-spine.006", "DEF-upper_arm.L.001"]);
        names.extend(sided(
            &["DEF-shoulder.#", "DEF-upper_arm.#", "DEF-forearm.#", "DEF-hand.#", "DEF-thigh.#", "DEF-shin.#", "DEF-foot.#", "DEF-toe.#", "DEF-f_ring.01.#", "DEF-f_ring.02.#", "DEF-f_ring.03.#", "DEF-thumb.01.#"],
            "L",
            "R",
        ));
        let (pairs, rig) = map(&names, RigHint::Auto);
        assert_eq!(rig, Some("rigify"));
        for (src, cs2) in [
            ("DEF-spine", "pelvis"),
            ("DEF-spine.001", "spine_0"),
            ("DEF-spine.002", "spine_2"),
            ("DEF-spine.003", "spine_3"),
            ("DEF-spine.004", "neck_0"),
            ("DEF-spine.006", "head_0"),
            ("DEF-shoulder.L", "clavicle_L"),
            ("DEF-upper_arm.R", "arm_upper_R"),
            ("DEF-forearm.L", "arm_lower_L"),
            ("DEF-thigh.R", "leg_upper_R"),
            ("DEF-shin.L", "leg_lower_L"),
            ("DEF-foot.R", "ankle_R"),
            ("DEF-toe.L", "ball_L"),
            ("DEF-f_ring.02.R", "finger_ring_1_R"),
            ("DEF-thumb.01.L", "finger_thumb_0_L"),
        ] {
            assert!(has(&pairs, src, cs2), "{src} -> {cs2} in {pairs:?}");
        }
        // the twist joint and DEF-spine.005 are left for the weight move
        assert!(!pairs.iter().any(|(a, _)| a == "DEF-upper_arm.L.001" || a == "DEF-spine.005"));
    }

    #[test]
    fn ue_mannequin() {
        let mut names = strings(&["root", "pelvis", "spine_01", "spine_02", "spine_03", "spine_04", "spine_05", "neck_01", "neck_02", "head", "ik_foot_root"]);
        names.extend(sided(
            &["clavicle_#", "upperarm_#", "lowerarm_#", "hand_#", "thigh_#", "calf_#", "foot_#", "ball_#", "index_metacarpal_#", "index_01_#", "index_02_#", "index_03_#", "thumb_01_#", "thumb_02_#", "thumb_03_#", "upperarm_twist_01_#"],
            "l",
            "r",
        ));
        let (pairs, rig) = map(&names, RigHint::Auto);
        assert_eq!(rig, Some("ue"));
        for (src, cs2) in [
            ("pelvis", "pelvis"),
            ("spine_01", "spine_0"),
            ("spine_02", "spine_1"),
            ("spine_04", "spine_2"),
            ("spine_05", "spine_3"),
            ("neck_01", "neck_0"),
            ("head", "head_0"),
            ("clavicle_l", "clavicle_L"),
            ("upperarm_r", "arm_upper_R"),
            ("lowerarm_l", "arm_lower_L"),
            ("calf_r", "leg_lower_R"),
            ("ball_l", "ball_L"),
            ("index_metacarpal_l", "finger_index_meta_L"),
            ("index_03_r", "finger_index_2_R"),
            ("thumb_01_l", "finger_thumb_0_L"),
        ] {
            assert!(has(&pairs, src, cs2), "{src} -> {cs2} in {pairs:?}");
        }
        assert!(!pairs.iter().any(|(a, _)| a == "spine_03" || a == "neck_02" || a == "root"));
    }

    #[test]
    fn vrm_humanoid() {
        let mut names = strings(&["hips", "spine", "chest", "upperChest", "neck", "head"]);
        names.extend(sided(
            &["#Shoulder", "#UpperArm", "#LowerArm", "#Hand", "#UpperLeg", "#LowerLeg", "#Foot", "#Toes", "#IndexProximal", "#IndexIntermediate", "#IndexDistal", "#ThumbMetacarpal", "#ThumbProximal", "#ThumbDistal"],
            "left",
            "right",
        ));
        let (pairs, rig) = map(&names, RigHint::Auto);
        assert_eq!(rig, Some("vrm"));
        for (src, cs2) in [
            ("hips", "pelvis"),
            ("spine", "spine_0"),
            ("chest", "spine_2"),
            ("upperChest", "spine_3"),
            ("leftShoulder", "clavicle_L"),
            ("rightLowerArm", "arm_lower_R"),
            ("leftUpperLeg", "leg_upper_L"),
            ("rightToes", "ball_R"),
            ("leftIndexProximal", "finger_index_0_L"),
            ("leftThumbMetacarpal", "finger_thumb_0_L"),
            ("leftThumbDistal", "finger_thumb_2_L"),
        ] {
            assert!(has(&pairs, src, cs2), "{src} -> {cs2} in {pairs:?}");
        }
    }

    #[test]
    fn mixamo_with_the_other_prefix_spellings() {
        let mut names = strings(&["mixamorig1:Hips", "mixamorig1:Spine", "mixamorig1:Neck", "mixamorig1:Head"]);
        names.extend(sided(
            &["mixamorig1:#Shoulder", "mixamorig1:#Arm", "mixamorig1:#ForeArm", "mixamorig1:#Hand", "mixamorig1:#UpLeg", "mixamorig1:#Leg", "mixamorig1:#Foot"],
            "Left",
            "Right",
        ));
        let (pairs, rig) = map(&names, RigHint::Mixamo);
        assert_eq!(rig, Some("mixamo"));
        assert_eq!(pairs.len(), 4 + 14);
        // one spine joint -> spine_0
        assert!(has(&pairs, "mixamorig1:Spine", "spine_0"));
    }
}

// ------------------------------------------------------------------------------ the tool

mod tool {
    use super::*;
    use cs2_mcp_common::config::CommonConfig;
    use cs2_mcp_common::mcp::McpServer;
    use cs2_mcp_common::tools::{CallFailure, ToolError};
    use hammer_mcp::state::HammerState;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub struct Tree {
        pub root: PathBuf,
        pub app: PathBuf,
    }

    impl Tree {
        /// `skeleton`: put the vnmskel next to the game as a loose file.
        pub fn new(tag: &str, skeleton: bool) -> Tree {
            let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("model-import-player-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
            let app = root.join("workshop/run/app");
            std::fs::create_dir_all(app.join("content/csgo_addons/mcp_test")).unwrap();
            std::fs::create_dir_all(root.join("src")).unwrap();
            if skeleton {
                let path = app.join("game/csgo").join(skeleton::SKELETON_COMPILED);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, common::vnmskel_c()).unwrap();
                let agent = app.join("game/csgo/agents/models/ctm_sas/ctm_sas.vmdl_c");
                std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
                std::fs::write(agent, common::agent_vmdl_c()).unwrap();
            }
            Tree { root, app }
        }

        pub fn source(&self, h: &Humanoid) -> String {
            let path = self.root.join("src/hero.glb");
            std::fs::write(&path, h.fixture.glb()).unwrap();
            path.display().to_string()
        }

        fn podman(&self) -> PathBuf {
            let dir = self.root.join("bin");
            std::fs::create_dir_all(&dir).unwrap();
            let body = r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps) printf '[{"Names":["cs2tools"],"State":"running"}]' ;;
  exec)
    last="${!#}"; rel="${last##* -i }"; rel="${rel#/app/content/}"
    out="$APP/game/${rel}_c"; mkdir -p "$(dirname "$out")"; touch "$out"
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s" ;;
esac
"#
            .replace("__APP__", &self.app.display().to_string());
            let script = dir.join("podman-ok");
            std::fs::write(&script, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        pub fn server(&self) -> McpServer<HammerState> {
            use clap::Parser;
            #[derive(Parser)]
            struct W {
                #[command(flatten)]
                c: CommonConfig,
            }
            let root = self.root.display().to_string();
            let podman = self.podman().display().to_string();
            let args = ["--deadline-ms", "5000", "--tool-budget-ms", "45000", "--project-root", &root, "--podman", &podman];
            let argv: Vec<&str> = [vec!["t"], common::hermetic(&args)].concat();
            hammer_mcp::build_server(&W::parse_from(argv).c).unwrap()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    pub async fn call(server: &McpServer<HammerState>, args: Value) -> Result<(Value, Vec<(String, Value)>), ToolError> {
        match server.call("model_import", args).await {
            Ok(out) => {
                let next = out.next.iter().flatten().map(|s| (s.tool.clone(), s.args.clone().unwrap_or(Value::Null))).collect();
                Ok((out.json, next))
            }
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }

    #[tokio::test]
    async fn a_humanoid_becomes_a_compiled_player_model() {
        let tree = Tree::new("ok", true);
        let source = tree.source(&humanoid::build(&Opts::default()));
        let server = tree.server();
        let (out, next) = call(&server, json!({"source": source, "kind": "player", "name": "hero"})).await.expect("imports");
        assert_eq!(out["kind"], json!("player"));
        assert_eq!(out["model"], json!("models/hero/hero.vmdl"));
        assert_eq!(out["compile"]["ok"], json!(true), "{out}");
        assert_eq!(out["physics"], json!("none"));
        assert_eq!(out["skeleton"]["bones"], json!(75));
        assert_eq!(out["skeleton"]["confidence"], json!(1.0));
        assert_eq!(out["retarget"]["rig"], json!("mixamo"));
        assert!(out["retarget"]["scale"].as_f64().unwrap() > 40.0);
        assert!(out["skeleton"]["mapped"].as_array().unwrap().iter().any(|m| m["cs2"] == "head_0" && m["how"] == "table:mixamo"));
        let files: Vec<&str> = out["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(files.contains(&"models/hero/hero.vmdl") && files.contains(&"models/hero/hero.smd"), "{files:?}");
        assert!(!files.iter().any(|f| f.ends_with("walk.smd")));
        let vmdl = std::fs::read_to_string(tree.app.join("content/csgo_addons/mcp_test/models/hero/hero.vmdl")).unwrap();
        assert!(vmdl.contains("worldmodel.vnmskel") && vmdl.contains("worldmodel.vnmgraph"));
        let place = next.iter().find(|(t, _)| t == "create_props").expect("create_props next");
        assert_eq!(place.1["props"][0], json!({"class": "prop_dynamic", "model": "models/hero/hero.vmdl", "keyValues": {}}));
        assert!(next.iter().any(|(t, _)| t == "look"));
        assert_eq!(out["markup"]["referenceAgent"], json!("agents/models/ctm_sas/ctm_sas.vmdl"));
        assert_eq!(out["markup"]["attachments"].as_array().unwrap().len(), 17);
        assert_eq!(out["markup"]["hitboxSets"], json!([{"name": "cstrike", "hitboxes": 19}]));
        assert_eq!(out["markup"]["moved"], json!([{"attachment": "primary", "from": "jiggle_primary", "to": "spine_3"}]));
        assert_eq!(out["markup"]["dropped"], json!([]));
        assert!(vmdl.contains("_class = \"HitboxCapsule\"") && vmdl.contains("parent_bone = \"wpn\""), "{vmdl}");
        let wear = next.iter().find(|(t, _)| t == "player_model").expect("player_model next");
        assert_eq!(wear.1, json!({"action": "set", "model": "models/hero/hero.vmdl"}));
    }

    #[tokio::test]
    async fn the_arguments_are_checked() {
        let tree = Tree::new("args", true);
        let source = tree.source(&humanoid::build(&Opts::default()));
        let server = tree.server();
        let base = |extra: Value| {
            let mut v = json!({"source": source, "kind": "player", "name": "hero", "dryRun": true});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            v
        };
        // agent: there is none to pick
        let e = call(&server, base(json!({"agent": "tm_phoenix"}))).await.unwrap_err();
        assert!(e.message.contains("unknown argument") && e.message.contains("agent"), "{}", e.message);
        let e = call(&server, base(json!({"upAxis": "z"}))).await.unwrap_err();
        assert!(e.message.contains("upAxis"), "{}", e.message);
        let e = call(&server, base(json!({"rig": "maya"}))).await.unwrap_err();
        assert!(e.message.contains("rig"), "{}", e.message);
        let e = call(&server, base(json!({"boneMap": {"mixamorig:Head": 4}}))).await.unwrap_err();
        assert!(e.message.contains("boneMap"), "{}", e.message);
        let e = call(&server, base(json!({"boneMap": {"mixamorig:Hed": "head_0"}}))).await.unwrap_err();
        assert!(e.message.contains("not in the file's skin"), "{}", e.message);
        assert!(e.data["didYouMean"].as_array().unwrap().iter().any(|n| n == "mixamorig:Head"), "{:?}", e.data);
        // boneMap/rig belong to player
        let mut prop = base(json!({"boneMap": {"a": "b"}}));
        prop["kind"] = json!("prop");
        let e = call(&server, prop).await.unwrap_err();
        assert!(e.message.contains("only valid with kind player"), "{}", e.message);
        let mut prop = base(json!({"referenceAgent": "agents/models/tm_phoenix/tm_phoenix.vmdl"}));
        prop["kind"] = json!("prop");
        let e = call(&server, prop).await.unwrap_err();
        assert!(e.message.contains("only valid with kind player"), "{}", e.message);
        let e = call(&server, base(json!({"referenceAgent": "../x.vmdl"}))).await.unwrap_err();
        assert!(e.message.contains("referenceAgent"), "{}", e.message);
        let e = call(&server, base(json!({"referenceAgent": "agents/models/tm_phoenix/tm_phoenix.vmdl"}))).await.unwrap_err();
        assert_eq!(e.type_, "AgentNotFound");
        assert!(e.message.contains("tm_phoenix.vmdl_c"), "{}", e.message);
        let (out, _) = call(&server, base(json!({"referenceAgent": "none"}))).await.expect("no markup");
        assert!(out.get("markup").is_none(), "{out}");
        // a fine dry run
        let (out, _) = call(&server, base(json!({"boneMap": {"mixamorig:Spine1": "spine_1"}, "rig": "mixamo"}))).await.expect("dry run");
        assert_eq!(out["written"], json!(false));
        assert!(out["skeleton"]["mapped"].as_array().unwrap().iter().any(|m| m["cs2"] == "spine_1" && m["how"] == "explicit"));
    }

    #[tokio::test]
    async fn typed_errors_name_the_problem() {
        let tree = Tree::new("errors", true);
        let server = tree.server();
        // NoSkin
        let cube = tree.root.join("src/cube.glb");
        std::fs::write(&cube, Fixture::cube().glb()).unwrap();
        let e = call(&server, json!({"source": cube.display().to_string(), "kind": "player", "dryRun": true})).await.unwrap_err();
        assert_eq!(e.type_, "NoSkin");
        // BoneMapIncomplete
        let h = humanoid::build(&Opts { omit: vec!["LeftForeArm"], ..Opts::default() });
        let e = call(&server, json!({"source": tree.source(&h), "kind": "player", "dryRun": true})).await.unwrap_err();
        assert_eq!(e.type_, "BoneMapIncomplete");
        assert_eq!(e.data["unmapped"], json!(["arm_lower_L"]));
        assert_eq!(e.data["required"].as_array().unwrap().len(), 18);
        assert!(e.message.contains("boneMap"), "{}", e.message);
        // SkeletonNotFound
        let bare = Tree::new("noskel", false);
        let server = bare.server();
        let e = call(&server, json!({"source": bare.source(&humanoid::build(&Opts::default())), "kind": "player", "dryRun": true})).await.unwrap_err();
        assert_eq!(e.type_, "SkeletonNotFound");
        assert_eq!(e.data["path"], json!(skeleton::SKELETON_COMPILED));
        assert!(e.message.contains("worldmodel.vnmskel_c"), "{}", e.message);
    }
}

#[test]
fn a_partial_finger_chain_keeps_its_segments() {
    // full chain: Index1..3 -> finger_index_0..2_L
    let h = humanoid::build(&Opts::default());
    let plan = plan_ok(&h);
    assert_eq!(mapped(&plan, "mixamorig:LeftHandIndex1").unwrap().0, "finger_index_0_L");
    assert_eq!(mapped(&plan, "mixamorig:LeftHandIndex3").unwrap().0, "finger_index_2_L");
    // only the base joint: it stays the base segment (live 2026-11-03: HandMiddle1 alone went to _2_)
    let h = humanoid::build(&Opts { omit: vec!["LeftHandIndex2", "LeftHandIndex3"], ..Opts::default() });
    let plan = plan_ok(&h);
    assert_eq!(mapped(&plan, "mixamorig:LeftHandIndex1").unwrap().0, "finger_index_0_L");
}
