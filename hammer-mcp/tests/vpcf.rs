//! The `hammer_mcp::vpcf` particle layer: parsing, the function arrays, addressing, edit ops,
//! schema validation, templates and the content-path rules.
//!
//! The fixtures under `fixtures/vpcf/` are `.vpcf` files in the writer's layout (CRLF, tabs, no
//! final newline) plus one hand-made schema snapshot. The live probe (`probe.vpcf`) is embedded
//! below as [`LIVE_PROBE`] byte for byte — it is LF with a trailing newline because it was
//! hand-written, and `live_probe_and_fixture_differ_only_in_line_endings` proves the fixture is
//! that same file in Valve's layout and nothing else.

use hammer_mcp::vmdl::{ContentPaths, VmdlError};
use hammer_mcp::vpcf::{
    self, known_schema, template, FunctionRef, Level, Op, Particle, Schema, ValidateOptions,
    VpcfError, ARRAYS, ROOT_CLASS, TEMPLATE_CLASSES,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

const PROBE: &str = include_str!("../fixtures/vpcf/probe.vpcf");
const WITH_CHILDREN: &str = include_str!("../fixtures/vpcf/with_children.vpcf");
/// The probe plus one `m_NamedValueLocals` (bridge type 8) entry in pet's own form, byte for byte:
/// pet's serialization of the probe after `add_variable` added one `mcp_float` variable (read back
/// live 2026-10-13, build 2000930; `target/orch/live2/namedvalue.saved.vpcf`).
const WITH_NAMEDVALUE: &str = include_str!("../fixtures/vpcf/with_namedvalue.vpcf");
const BAD_ROOT: &str = include_str!("../fixtures/vpcf/bad_root.vpcf");
const NOT_KV3: &str = include_str!("../fixtures/vpcf/not_kv3.vpcf");
const SCHEMA_TEST: &str = include_str!("../fixtures/vpcf/schema-test.json");
/// The placeholder the crate shipped before the live snapshot (`placeholder:true`, no classes).
const PLACEHOLDER: &str = include_str!("../fixtures/vpcf/schema-placeholder.json");

/// The live hand-written probe
/// (`workshop/run/app/content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf`), byte for
/// byte: LF line endings and a trailing newline. It is embedded here rather than copied into
/// `fixtures/` because the fixture has to be in the writer's own layout; the test below checks
/// that the two are the same text.
const LIVE_PROBE: &str = r#"<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->
{
	_class = "CParticleSystemDefinition"
	m_nMaxParticles = 32
	m_Emitters = 
	[
		{
			_class = "C_OP_ContinuousEmitter"
		},
	]
	m_Initializers = 
	[
		{
			_class = "C_INIT_CreateWithinSphereTransform"
		},
	]
	m_Operators = 
	[
		{
			_class = "C_OP_BasicMovement"
		},
		{
			_class = "C_OP_Decay"
		},
	]
	m_Renderers = 
	[
		{
			_class = "C_OP_RenderSprites"
		},
	]
}
"#;

// ---------------------------------------------------------------- helpers

fn parse(text: &str) -> Particle {
    Particle::parse(text).unwrap_or_else(|error| panic!("{error}"))
}

fn reference(text: &str) -> FunctionRef {
    text.parse().unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn op(json: Value) -> Op {
    serde_json::from_value(json).unwrap_or_else(|error| panic!("{error}"))
}

fn ops_value(json: Value) -> Vec<Op> {
    serde_json::from_value(json).unwrap_or_else(|error| panic!("{error}"))
}

/// The hand-made test snapshot (`fixtures/vpcf/schema-test.json`).
fn schema() -> Schema {
    let json: Value = serde_json::from_str(SCHEMA_TEST).expect("schema-test.json is JSON");
    Schema::from_json(&json).unwrap_or_else(|error| panic!("{error}"))
}

/// A `.vpcf` built from a raw KV3 body (the parser normalises the `\n` line endings).
fn vpcf(text: &str) -> Particle {
    let document = format!(
        "{}\n{{\n\t_class = \"{ROOT_CLASS}\"\n{text}}}\n",
        vpcf::HEADER
    );
    parse(&document)
}

/// The `_class` of every function, in [`Particle::list`] order.
fn classes(particle: &Particle) -> Vec<String> {
    particle
        .list(None, None)
        .into_iter()
        .map(|function| function.class.unwrap_or_default())
        .collect()
}

/// `fixtures/vpcf/probe.vpcf` with a `m_nBehaviorVersion = <value>` line inserted directly after
/// the root `_class` line (the position the template writes it at; see `CURRENT_BEHAVIOR_VERSION`).
/// The probe fixture itself is the live-compiled reference file and stays without the field.
fn probe_with_behavior_version(value: &str) -> String {
    let line = "\t_class = \"CParticleSystemDefinition\"\r\n";
    assert!(PROBE.contains(line), "the probe fixture changed");
    PROBE.replacen(
        line,
        &format!("{line}\tm_nBehaviorVersion = {value}\r\n"),
        1,
    )
}

/// The issues apart from the behavior-version warning: every pre-`m_nBehaviorVersion` file
/// (all fixture `.vpcf`s, which predate the field) implies exactly that one warning, so body
/// tests can assert "clean apart from the version" — the dedicated named tests below.
fn without_version_warning(issues: &[vpcf::Issue]) -> Vec<vpcf::Issue> {
    issues
        .iter()
        .filter(|issue| !(issue.path == "m_nBehaviorVersion" && issue.level == Level::Warning))
        .cloned()
        .collect()
}

/// A fresh scratch directory under `target/tmp` (never `/tmp`), unique per test.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("vpcf-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::canonicalize(&dir).expect("scratch dir canonical")
}

/// `(message path, level, message)` for readable assertions.
fn messages(issues: &[vpcf::Issue]) -> Vec<(String, Level, String)> {
    issues
        .iter()
        .map(|issue| (issue.path.clone(), issue.level, issue.msg.clone()))
        .collect()
}

/// The single issue at `path`, or a panic naming what was found instead.
fn issue_at(issues: &[vpcf::Issue], path: &str) -> vpcf::Issue {
    let found: Vec<&vpcf::Issue> = issues.iter().filter(|issue| issue.path == path).collect();
    assert_eq!(found.len(), 1, "expected one issue at {path}, got {issues:?}");
    found[0].clone()
}

// ---------------------------------------------------------------- parse / write

#[test]
fn probe_fixture_round_trips_byte_for_byte() {
    let particle = parse(PROBE);
    assert_eq!(particle.write(), PROBE);
    assert_eq!(cs2_kv3::write(&cs2_kv3::parse(PROBE).unwrap()), PROBE);
    assert_eq!(particle.doc.header.format, "vpcf66");
    assert_eq!(particle.root().get("_class").and_then(cs2_kv3::Value::as_str), Some(ROOT_CLASS));
}

#[test]
fn live_probe_and_fixture_differ_only_in_line_endings() {
    // The live file is the hand-written probe: tabs, the trailing space after `m_Emitters = `,
    // LF line endings and a final newline.
    assert!(!LIVE_PROBE.contains('\r'), "the live probe has CR bytes");
    assert!(LIVE_PROBE.ends_with("}\n"), "the live probe does not end with `}}\\n`");
    assert!(LIVE_PROBE.contains("\tm_Emitters = \n"), "the live probe lost Valve's trailing space");
    // The fixture is that text in the writer's layout.
    assert!(PROBE.contains("\r\n"));
    assert!(PROBE.ends_with('}'));
    assert!(PROBE.contains("\tm_Emitters = \r\n"));
    // Writing the live text in its own layout reproduces it byte for byte, so the writer changes
    // nothing but the line endings ...
    let reformatted =
        cs2_kv3::write_styled(&cs2_kv3::parse(LIVE_PROBE).unwrap(), cs2_kv3::Style::LF);
    assert_eq!(reformatted, LIVE_PROBE, "the writer changed more than the line endings");
    // ... and `Particle::write` (Valve's layout) turns the live file into the fixture exactly.
    assert_eq!(Particle::parse(LIVE_PROBE).unwrap().write(), PROBE);
}

#[test]
fn with_children_fixture_round_trips_and_parses_the_flagged_child_ref() {
    let particle = parse(WITH_CHILDREN);
    assert_eq!(particle.write(), WITH_CHILDREN);
    let children = particle.list(Some("m_Children"), None);
    assert_eq!(children.len(), 1);
    // Valve's own files may spell a structural entry without a `_class`, and that is pet's file
    // form of `m_Children` too (live fact 2026-10-13: entries are
    // `{ m_ChildRef = resource:"…" }`, no `_class`), so `None` is what this layer accepts.
    assert_eq!(children[0].class, None);
    assert_eq!(children[0].r#type, 7);
    assert_eq!(children[0].field_keys, ["m_ChildRef"]);
    let entry = particle.function(&FunctionRef::new("m_Children", 0)).unwrap();
    assert_eq!(
        cs2_kv3::to_json(entry.get("m_ChildRef").unwrap()),
        json!({"$flag": "resource", "value": "particles/mcp_probe/probe.vpcf"})
    );
}

#[test]
fn not_kv3_fails_to_parse_and_bad_root_parses_but_does_not_validate() {
    assert!(matches!(Particle::parse(NOT_KV3), Err(VpcfError::Parse(_))));
    assert!(matches!(Particle::parse("not kv3 at all"), Err(VpcfError::Parse(_))));

    // A wrong root `_class` parses (the `particle_validate` tool has to report it as an issue,
    // not fail on the read) and the structural pass names it.
    let particle = parse(BAD_ROOT);
    let issues = particle.validate_structure();
    let root = issue_at(&issues, "/");
    assert!(root.level.is_error());
    assert!(
        root.msg.contains("CParticleSystem\"") && root.msg.contains(ROOT_CLASS),
        "{issues:?}"
    );
    assert_eq!(particle.validate(&schema(), ValidateOptions::default()), issues);
}

// ---------------------------------------------------------------- arrays / refs

#[test]
fn the_array_table_is_the_shared_type_mapping() {
    assert_eq!(ARRAYS.len(), 9);
    for (index, (r#type, key, element)) in ARRAYS.iter().enumerate() {
        assert_eq!(*r#type, index);
        assert_eq!(vpcf::array_type(key), Some(*r#type));
        assert_eq!(vpcf::array_key(*r#type), Some(*key));
        assert_eq!(vpcf::array_element_class(key), *element);
    }
    assert_eq!(vpcf::array_type("m_Bogus"), None);
    assert_eq!(vpcf::array_key(9), None);
    assert_eq!(vpcf::array_element_class("m_Operators"), None);
    assert_eq!(
        vpcf::array_element_class("m_Children"),
        Some("ParticleChildrenInfo_t")
    );
    assert_eq!(
        vpcf::array_element_class("m_NamedValueLocals"),
        Some("ParticleNamedValueSource_t")
    );
    assert_eq!(vpcf::array_keys().len(), 9);
}

#[test]
fn function_refs_parse_and_display() {
    let entry = reference("m_Operators/2");
    assert_eq!(entry, FunctionRef::new("m_Operators", 2));
    assert_eq!(entry.to_string(), "m_Operators/2");
    assert!(!entry.is_root());

    let root = reference("/");
    assert!(root.is_root());
    assert_eq!(root.to_string(), "/");
    assert_eq!(FunctionRef::root(), FunctionRef::default());
    assert!(reference("").is_root());

    for bad in ["m_Operators", "m_Operators/x", "/2", "m_Operators/-1"] {
        let error = bad.parse::<FunctionRef>().unwrap_err();
        assert!(matches!(error, VpcfError::Path(_)), "{bad}: {error:?}");
    }
}

#[test]
fn list_gives_the_probe_functions_with_types_and_indices() {
    let particle = parse(PROBE);
    let all = particle.list(None, None);
    let rows: Vec<(&str, usize, usize, Option<&str>)> = all
        .iter()
        .map(|function| {
            (
                function.array.as_str(),
                function.r#type,
                function.index,
                function.class.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("m_Emitters", 1, 0, Some("C_OP_ContinuousEmitter")),
            (
                "m_Initializers",
                2,
                0,
                Some("C_INIT_CreateWithinSphereTransform")
            ),
            ("m_Operators", 3, 0, Some("C_OP_BasicMovement")),
            ("m_Operators", 3, 1, Some("C_OP_Decay")),
            ("m_Renderers", 6, 0, Some("C_OP_RenderSprites")),
        ]
    );
    assert!(all.iter().all(|function| function.field_keys.is_empty()));
    assert_eq!(particle.list(Some("m_Operators"), None).len(), 2);
    assert_eq!(particle.list(None, Some("C_OP_Decay")).len(), 1);
    assert_eq!(particle.list(None, Some("C_OP_Decay"))[0].index, 1);
    // Filtering is never an error: absent or unknown arrays/classes simply match nothing.
    assert!(particle.list(Some("m_Children"), None).is_empty());
    assert!(particle.list(Some("m_Nope"), None).is_empty());
    assert!(particle.list(None, Some("C_OP_Nope")).is_empty());
}

#[test]
fn function_json_and_root_json_describe_the_probe() {
    let particle = parse(PROBE);

    let counted = particle.root_json(false);
    assert_eq!(counted["_class"], json!(ROOT_CLASS));
    assert_eq!(counted["m_nMaxParticles"], json!(32));
    assert_eq!(counted["m_Emitters"], json!(1));
    assert_eq!(counted["m_Operators"], json!(2));
    assert_eq!(counted["m_Renderers"], json!(1));
    // The probe has no `m_Children`: nothing is invented for an absent array.
    assert!(counted.get("m_Children").is_none());

    let full = particle.root_json(true);
    assert_eq!(full["m_Operators"].as_array().unwrap().len(), 2);
    assert_eq!(full["m_Operators"][0]["_class"], json!("C_OP_BasicMovement"));
    assert_eq!(full["m_Emitters"][0]["_class"], json!("C_OP_ContinuousEmitter"));

    let entry = particle.function_json(&reference("m_Operators/1")).unwrap();
    assert_eq!(entry["path"], json!("m_Operators/1"));
    assert_eq!(entry["array"], json!("m_Operators"));
    assert_eq!(entry["type"], json!(3));
    assert_eq!(entry["index"], json!(1));
    assert_eq!(entry["_class"], json!("C_OP_Decay"));
    assert_eq!(entry["fields"], json!({}));

    let root = particle.function_json(&FunctionRef::root()).unwrap();
    assert_eq!(root["path"], json!("/"));
    assert_eq!(root["array"], Value::Null);
    assert_eq!(root["type"], Value::Null);
    assert_eq!(root["index"], Value::Null);
    assert_eq!(root["_class"], json!(ROOT_CLASS));
    assert_eq!(root["fields"]["m_nMaxParticles"], json!(32));
    assert_eq!(root["fields"]["m_Operators"].as_array().unwrap().len(), 2);

    let error = particle.function_json(&reference("m_Operators/9")).unwrap_err();
    assert!(
        matches!(error, VpcfError::FunctionNotFound(ref what) if what == "m_Operators/9"),
        "{error:?}"
    );
    let error = particle.function_json(&reference("m_Bogus/0")).unwrap_err();
    assert!(matches!(error, VpcfError::UnknownArray(ref what) if what == "m_Bogus"), "{error:?}");
}

// ---------------------------------------------------------------- ops

#[test]
fn op_json_shapes_deserialize() {
    assert_eq!(
        op(json!({"op": "add_function", "array": "m_Operators", "function": {"_class": "X"}})),
        Op::AddFunction {
            array: "m_Operators".to_string(),
            index: None,
            function: json!({"_class": "X"}),
        }
    );
    assert_eq!(
        op(json!({"op": "remove_function", "array": "m_Operators", "index": 1})),
        Op::RemoveFunction {
            array: "m_Operators".to_string(),
            index: 1,
        }
    );
    assert_eq!(
        op(json!({"op": "move_function", "array": "m_Operators", "index": 0, "to_index": 2})),
        Op::MoveFunction {
            array: "m_Operators".to_string(),
            index: 0,
            to_index: 2,
            to_array: None,
        }
    );
    assert_eq!(
        op(json!({"op": "set_field", "array": "m_Operators", "index": 0, "key": "k", "value": [1, 2]})),
        Op::SetField {
            array: "m_Operators".to_string(),
            index: 0,
            key: "k".to_string(),
            value: json!([1, 2]),
        }
    );
    assert_eq!(
        op(json!({"op": "remove_field", "array": "m_Operators", "index": 0, "key": "k"})),
        Op::RemoveField {
            array: "m_Operators".to_string(),
            index: 0,
            key: "k".to_string(),
        }
    );
    assert_eq!(
        op(json!({"op": "set_root_field", "key": "m_nMaxParticles", "value": 8})),
        Op::SetRootField {
            key: "m_nMaxParticles".to_string(),
            value: json!(8),
        }
    );
    assert_eq!(
        op(json!({"op": "remove_root_field", "key": "m_nMaxParticles"})),
        Op::RemoveRootField {
            key: "m_nMaxParticles".to_string(),
        }
    );
    assert_eq!(
        op(json!({"op": "add_child", "child": {"m_ChildRef": 1}})),
        Op::AddChild {
            index: None,
            child: json!({"m_ChildRef": 1}),
        }
    );
    assert_eq!(
        op(json!({"op": "remove_child", "index": 0})),
        Op::RemoveChild { index: 0 }
    );
    assert_eq!(op(json!({"op": "remove_child", "index": 0})).name(), "remove_child");
    assert!(serde_json::from_value::<Op>(json!({"op": "no_such_op", "index": 0})).is_err());
}

#[test]
fn add_function_uses_valves_key_order_and_creates_a_missing_array() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Operators",
            "function": {"_class": "C_OP_Decay", "m_fDrag": 1.5, "m_bEnabled": false}
        }])))
        .unwrap();
    assert_eq!(results[0].op, "add_function");
    assert_eq!(results[0].path, "m_Operators/2");

    let entry = particle.function(&reference("m_Operators/2")).unwrap();
    assert_eq!(
        entry.keys().collect::<Vec<_>>(),
        ["_class", "m_bEnabled", "m_fDrag"],
        "serde_json order is alphabetical: _class first, then the fields"
    );
    let written = particle.write();
    assert!(written.contains("\t\t\t_class = \"C_OP_Decay\"\r\n\t\t\tm_bEnabled = false\r\n\t\t\tm_fDrag = 1.5\r\n"), "{written}");
    assert_eq!(parse(&written).write(), written);

    // `index` inserts; an absent array key is created.
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Emitters",
            "index": 0,
            "function": {"_class": "C_OP_ContinuousEmitter"}
        }])))
        .unwrap();
    assert_eq!(particle.list(Some("m_Emitters"), None).len(), 2);

    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_ForceGenerators",
            "function": {"_class": "C_OP_BasicMovement"}
        }])))
        .unwrap();
    assert_eq!(results[0].path, "m_ForceGenerators/0");
    assert_eq!(particle.list(Some("m_ForceGenerators"), None).len(), 1);
    assert!(particle.write().contains("\tm_ForceGenerators = \r\n"), "the array key was added");
}

#[test]
fn add_function_negative_cases_leave_the_particle_alone() {
    for (ops, expected, at) in [
        (
            json!([{"op": "add_function", "array": "m_Bogus", "function": {"_class": "X"}}]),
            "is not a function array",
            0,
        ),
        (
            json!([{"op": "add_function", "array": "m_Operators", "function": {"m_fDrag": 1.0}}]),
            "need a string _class",
            0,
        ),
        (
            json!([{"op": "add_function", "array": "m_Operators", "function": 7}]),
            "function must be an object",
            0,
        ),
        (
            json!([{"op": "add_function", "array": "m_Operators", "index": 9, "function": {"_class": "C_OP_Decay"}}]),
            "past the end of m_Operators",
            0,
        ),
        (
            // `_class` must be a string everywhere, including in the structural arrays.
            json!([{"op": "add_child", "child": {"_class": 3}}]),
            "_class must be a string",
            0,
        ),
    ] {
        let mut particle = parse(PROBE);
        let before = particle.write();
        let error = particle.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(matches!(error, VpcfError::BadOp { index, .. } if index == at), "{error:?}");
        assert_eq!(particle.write(), before, "a refused add_function changed the particle");
    }
}

#[test]
fn add_child_does_not_require_a_class() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "add_child",
            "child": {"m_ChildRef": {"$flag": "resource", "value": "particles/mcp_probe/probe.vpcf"}}
        }])))
        .unwrap();
    assert_eq!(results[0].op, "add_child");
    assert_eq!(results[0].path, "m_Children/0");
    let children = particle.list(Some("m_Children"), None);
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].class, None);
    assert!(
        particle.write().contains("m_ChildRef = resource:\"particles/mcp_probe/probe.vpcf\"\r\n"),
        "{}",
        particle.write()
    );
}

#[test]
fn remove_function_reports_the_reference_it_removed() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{"op": "remove_function", "array": "m_Operators", "index": 0}])))
        .unwrap();
    assert_eq!(results[0].path, "m_Operators/0");
    assert_eq!(
        classes(&particle),
        [
            "C_OP_ContinuousEmitter",
            "C_INIT_CreateWithinSphereTransform",
            "C_OP_Decay",
            "C_OP_RenderSprites",
        ]
    );
    // An array that loses its last entry keeps its key: [`Particle::write`] renders `[  ]`.
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([
            {"op": "remove_function", "array": "m_Operators", "index": 1},
            {"op": "remove_function", "array": "m_Operators", "index": 0},
        ])))
        .unwrap();
    assert!(particle.list(Some("m_Operators"), None).is_empty());
    assert!(particle.write().contains("\tm_Operators = [  ]\r\n"), "{}", particle.write());

    let mut particle = parse(PROBE);
    let before = particle.write();
    let error = particle
        .apply(&ops_value(json!([{"op": "remove_function", "array": "m_Operators", "index": 9}])))
        .unwrap_err();
    assert!(error.to_string().contains("no entry at m_Operators/9"), "{error}");
    assert_eq!(particle.write(), before);

    let error = particle
        .apply(&ops_value(json!([{"op": "remove_child", "index": 0}])))
        .unwrap_err();
    assert!(error.to_string().contains("no entry at m_Children/0"), "{error}");
    assert_eq!(particle.write(), before);
}

#[test]
fn move_function_moves_within_and_across_arrays() {
    // Within one array: m_Operators/0 -> index 1 (after the entry was taken out).
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "move_function",
            "array": "m_Operators",
            "index": 0,
            "to_index": 1
        }])))
        .unwrap();
    assert_eq!(results[0].path, "m_Operators/1");
    assert_eq!(particle.list(Some("m_Operators"), None)[0].class.as_deref(), Some("C_OP_Decay"));

    // Across arrays, appended by an explicit index.
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "move_function",
            "array": "m_Operators",
            "index": 1,
            "to_index": 1,
            "to_array": "m_Emitters"
        }])))
        .unwrap();
    assert_eq!(results[0].path, "m_Emitters/1");
    let emitters: Vec<Option<String>> = particle
        .list(Some("m_Emitters"), None)
        .into_iter()
        .map(|function| function.class)
        .collect();
    assert_eq!(
        emitters,
        [
            Some("C_OP_ContinuousEmitter".to_string()),
            Some("C_OP_Decay".to_string())
        ]
    );
    assert_eq!(particle.list(Some("m_Operators"), None).len(), 1);

    for (ops, expected) in [
        (
            json!([{"op": "move_function", "array": "m_Operators", "index": 9, "to_index": 0}]),
            "no entry at m_Operators/9",
        ),
        (
            json!([{"op": "move_function", "array": "m_Operators", "index": 0, "to_index": 9}]),
            "past the end of m_Operators",
        ),
        (
            json!([{"op": "move_function", "array": "m_Operators", "index": 0, "to_index": 0, "to_array": "m_Bogus"}]),
            "is not a function array",
        ),
        (
            // The destination of a same-array move has one entry less after the removal.
            json!([{"op": "move_function", "array": "m_Operators", "index": 0, "to_index": 2}]),
            "index 2 is past the end",
        ),
    ] {
        let mut particle = parse(PROBE);
        let before = particle.write();
        let error = particle.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(particle.write(), before, "a refused move changed the particle");
    }
}

#[test]
fn set_field_and_remove_field_change_one_entry() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([
            {"op": "set_field", "array": "m_Operators", "index": 0, "key": "m_fDrag", "value": 0.25},
            {"op": "set_field", "array": "m_Operators", "index": 0, "key": "_class", "value": "C_OP_Decay"},
            {"op": "remove_field", "array": "m_Operators", "index": 0, "key": "m_fDrag"},
        ])))
        .unwrap();
    assert_eq!(results[0].path, "m_Operators/0");
    assert_eq!(results[2].path, "m_Operators/0");
    // A new field is appended after the existing keys; `_class` keeps its position.
    let entry = particle.function(&reference("m_Operators/0")).unwrap();
    assert_eq!(entry.keys().collect::<Vec<_>>(), ["_class"]);
    assert_eq!(entry.get("_class").and_then(cs2_kv3::Value::as_str), Some("C_OP_Decay"));
    assert!(particle.write().contains("\t\t\t_class = \"C_OP_Decay\"\r\n"), "{}", particle.write());

    for (ops, expected) in [
        (
            json!([{"op": "set_field", "array": "m_Operators", "index": 9, "key": "k", "value": 1}]),
            "no entry at m_Operators/9",
        ),
        (
            json!([{"op": "remove_field", "array": "m_Operators", "index": 0, "key": "m_nope"}]),
            "has no field \"m_nope\"",
        ),
        (
            json!([{"op": "set_field", "array": "m_Bogus", "index": 0, "key": "k", "value": 1}]),
            "is not a function array",
        ),
    ] {
        let mut particle = parse(PROBE);
        let before = particle.write();
        let error = particle.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(particle.write(), before);
    }
}

#[test]
fn set_and_remove_root_field_refuse_structure() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([
            {"op": "set_root_field", "key": "m_flConstantRadius", "value": 0.5},
            {"op": "set_root_field", "key": "m_nMaxParticles", "value": 64},
            {"op": "remove_root_field", "key": "m_flConstantRadius"},
        ])))
        .unwrap();
    assert_eq!(results[0].path, "/");
    assert!(particle.write().contains("\tm_nMaxParticles = 64\r\n"), "{}", particle.write());
    assert!(!particle.write().contains("m_flConstantRadius"), "{}", particle.write());

    for (ops, expected) in [
        (
            json!([{"op": "set_root_field", "key": "m_Emitters", "value": []}]),
            "is a function array",
        ),
        (
            json!([{"op": "set_root_field", "key": "_class", "value": "X"}]),
            "\"_class\" is structural",
        ),
        (
            json!([{"op": "remove_root_field", "key": "m_Operators"}]),
            "is a function array",
        ),
        (
            json!([{"op": "remove_root_field", "key": "_class"}]),
            "\"_class\" is structural",
        ),
        (
            json!([{"op": "remove_root_field", "key": "m_nope"}]),
            "the root has no field \"m_nope\"",
        ),
    ] {
        let mut particle = parse(PROBE);
        let before = particle.write();
        let error = particle.apply(&ops_value(ops)).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(matches!(error, VpcfError::BadOp { index: 0, .. }), "{error:?}");
        assert_eq!(particle.write(), before, "a refused root op changed the particle");
    }
}

#[test]
fn apply_is_atomic() {
    let mut particle = parse(PROBE);
    let before = particle.write();
    let error = particle
        .apply(&ops_value(json!([
            {"op": "set_field", "array": "m_Operators", "index": 1, "key": "m_fDrag", "value": 0.25},
            {"op": "remove_function", "array": "m_Operators", "index": 9},
        ])))
        .unwrap_err();
    assert!(matches!(error, VpcfError::BadOp { index: 1, .. }), "{error:?}");
    assert_eq!(particle.write(), before, "op 1 succeeded but op 2 failed: nothing may change");
    assert_eq!(classes(&particle).len(), 5);

    // The same ops minus the failing one do land.
    let results = particle
        .apply(&ops_value(json!([
            {"op": "set_field", "array": "m_Operators", "index": 1, "key": "m_fDrag", "value": 0.25},
            {"op": "remove_function", "array": "m_Renderers", "index": 0},
        ])))
        .unwrap();
    assert_eq!(results.len(), 2);
    assert!(particle.write().contains("\t\t\tm_fDrag = 0.25\r\n"), "{}", particle.write());
    assert!(particle
        .list(None, None)
        .iter()
        .all(|function| function.array != "m_Renderers"));
}

// ---------------------------------------------------------------- validate

#[test]
fn validate_accepts_the_probe_with_the_test_schema() {
    // The probe fixture predates `m_nBehaviorVersion`, so its validation carries exactly the
    // behavior-version warning (dedicated named tests below); strict never fails on a warning.
    let particle = parse(PROBE);
    let issues = particle.validate(&schema(), ValidateOptions::default());
    assert!(
        without_version_warning(&issues).is_empty(),
        "{:?}",
        messages(&issues)
    );
    assert_eq!(issues.len(), 1);
    assert_eq!(issue_at(&issues, "m_nBehaviorVersion").level, Level::Warning);
    assert!(particle.validate_strict(&schema(), true).is_ok());
    assert!(particle.validate_strict(&schema(), false).is_ok());
}

#[test]
fn validate_reports_an_unknown_class_with_the_function_path() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Operators",
            "index": 0,
            "function": {"_class": "C_OP_NoSuchThing"}
        }])))
        .unwrap();
    let issues = particle.validate(&schema(), ValidateOptions::default());
    let issue = issue_at(&issues, "m_Operators/0");
    assert!(issue.level.is_error());
    assert!(issue.msg.contains("unknown function class \"C_OP_NoSuchThing\""), "{issue:?}");
    let error = particle.validate_strict(&schema(), true).unwrap_err();
    assert!(error.to_string().contains("C_OP_NoSuchThing"), "{error}");
}

#[test]
fn validate_reports_a_class_in_the_wrong_array_and_names_the_expected_ones() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Emitters",
            "index": 0,
            "function": {"_class": "C_OP_Decay"}
        }])))
        .unwrap();
    let schema = schema();
    // `C_OP_Decay` is registered for the operator arrays only (and for two of them).
    assert_eq!(schema.registration_types("C_OP_Decay"), vec![3, 4]);
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Emitters/0");
    assert!(issue.level.is_error());
    assert!(issue.msg.contains("not registered for m_Emitters"), "{issue:?}");
    assert!(issue.msg.contains("m_Operators") && issue.msg.contains("m_ForceGenerators"), "{issue:?}");
}

#[test]
fn validate_reports_an_unknown_field_and_accepts_a_base_class_field() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Operators",
            "index": 0,
            "function": {"_class": "C_OP_BasicMovement", "m_flOpStartFadeInTime": 0.5}
        }])))
        .unwrap();
    // `m_flOpStartFadeInTime` is declared in the base class `CParticleFunctionOperator`; the
    // snapshot's fields are flattened, so the entry validates.
    let schema = schema();
    let info = schema.class("C_OP_BasicMovement").unwrap();
    let field = info
        .fields
        .iter()
        .find(|field| field.name == "m_flOpStartFadeInTime")
        .expect("the flattened field list has the base field");
    assert_eq!(field.declared_in, "CParticleFunctionOperator");
    assert!(info.bases.contains(&"CParticleFunctionOperator".to_string()));
    let issues = particle.validate(&schema, ValidateOptions::default());
    assert!(
        without_version_warning(&issues).is_empty(),
        "{:?}",
        messages(&issues)
    );

    // An unknown field is an error naming the field and the class.
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Operators",
            "index": 0,
            "function": {"_class": "C_OP_BasicMovement", "m_bogusField": 1.0}
        }])))
        .unwrap();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Operators/0");
    assert!(issue.level.is_error());
    assert!(
        issue.msg.contains("unknown field \"m_bogusField\"") && issue.msg.contains("C_OP_BasicMovement"),
        "{issue:?}"
    );
}

#[test]
fn validate_reports_an_unknown_root_field() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{"op": "set_root_field", "key": "m_bogusField", "value": 1.0}])))
        .unwrap();
    let issues = particle.validate(&schema(), ValidateOptions::default());
    let issue = issue_at(&issues, "m_bogusField");
    assert!(issue.level.is_error());
    assert!(issue.msg.contains("unknown root field"), "{issue:?}");
    // The arrays themselves are structure, not "unknown root fields"; the only issue on the
    // untouched probe is the behavior-version warning.
    let issues = parse(PROBE).validate(&schema(), ValidateOptions::default());
    assert!(
        without_version_warning(&issues).is_empty(),
        "{:?}",
        messages(&issues)
    );
    assert_eq!(issue_at(&issues, "m_nBehaviorVersion").level, Level::Warning);
}

#[test]
fn validate_warns_on_an_obsolete_class_and_strict_can_make_it_an_error() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_function",
            "array": "m_Operators",
            "index": 0,
            "function": {"_class": "C_OP_TestObsolete"}
        }])))
        .unwrap();
    let schema = schema();
    assert!(schema.is_obsolete(3, "C_OP_TestObsolete"));

    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Operators/0");
    assert_eq!(issue.level, Level::Warning, "{issue:?}");
    assert!(issue.msg.contains("obsolete"), "{issue:?}");
    assert!(issue.msg.contains("MObsoleteParticleFunction"), "{issue:?}");
    // A warning never fails the strict check while obsolete classes are allowed ...
    assert!(particle.validate_strict(&schema, true).is_ok());
    // ... and `allow_obsolete: false` turns it into the error.
    let error = particle.validate_strict(&schema, false).unwrap_err();
    assert!(matches!(error, VpcfError::Invalid(ref issues) if issues.len() == 1), "{error:?}");
    assert!(error.to_string().contains("obsolete"), "{error}");
}

#[test]
fn validate_checks_the_structural_arrays_against_their_element_class() {
    let schema = schema();
    let issues = parse(WITH_CHILDREN).validate(&schema, ValidateOptions::default());
    assert!(
        without_version_warning(&issues).is_empty(),
        "with_children expects only the behavior-version warning: {issues:?}"
    );
    assert_eq!(issues.len(), 1);

    // A `_class` equal to the element class is accepted; an unknown field is not.
    let mut particle = parse(WITH_CHILDREN);
    particle
        .apply(&ops_value(json!([{
            "op": "add_child",
            "child": {"_class": "ParticleChildrenInfo_t", "m_ChildRef": {"$flag": "resource", "value": "particles/x.vpcf"}}
        }])))
        .unwrap();
    let issues = particle.validate(&schema, ValidateOptions::default());
    assert!(without_version_warning(&issues).is_empty(), "{issues:?}");

    let mut particle = parse(WITH_CHILDREN);
    particle
        .apply(&ops_value(json!([{"op": "add_child", "child": {"m_bogus": 1}}])))
        .unwrap();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Children/1");
    assert!(
        issue.msg.contains("unknown field \"m_bogus\"") && issue.msg.contains("ParticleChildrenInfo_t"),
        "{issue:?}"
    );

    let mut particle = parse(WITH_CHILDREN);
    particle
        .apply(&ops_value(json!([{"op": "add_child", "child": {"_class": "NotTheStruct"}}])))
        .unwrap();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Children/1");
    assert!(
        issue.msg.contains("is not the element class \"ParticleChildrenInfo_t\""),
        "{issue:?}"
    );
}

// ------------------------------------------- m_NamedValueLocals (type 8, second pass)
//
// pet's own form, read back live (2026-10-13, build 2000930; `target/orch/live-facts.md`
// "m_NamedValueLocals (the answer)"): no `_class` (like `m_Children`), one entry per variable with
// `m_Name`, then `m_IsPublic` only when it is `false`, then `m_DefaultConfig`, then `m_ValueType`.
// `with_namedvalue.vpcf` holds that text byte for byte, and the `add_variable` test below proves
// the op still reproduces it. A `// pet's form` comment cannot live inside the fixture: the KV3
// writer drops comments.

#[test]
fn namedvalue_fixture_round_trips_byte_identical() {
    let particle = parse(WITH_NAMEDVALUE);
    assert_eq!(particle.write(), WITH_NAMEDVALUE, "the writer changed pet's fixture");

    let entries = particle.list(Some(vpcf::namedvalue::KEY), None);
    assert_eq!(entries.len(), 1, "the fixture holds exactly one variable");
    assert_eq!(entries[0].array, "m_NamedValueLocals");
    assert_eq!(entries[0].r#type, 8);
    // pet carries no `_class` and omits `m_IsPublic = true` (its default).
    assert_eq!(entries[0].class, None);
    assert_eq!(entries[0].field_keys, ["m_Name", "m_DefaultConfig", "m_ValueType"]);

    // The fixture is schema-clean against the hand-made test snapshot: the only issue is the
    // behavior-version warning every pre-`m_nBehaviorVersion` fixture implies.
    let issues = particle.validate(&schema(), ValidateOptions::default());
    assert!(without_version_warning(&issues).is_empty(), "{:?}", messages(&issues));
}

#[test]
fn add_variable_builds_pets_form() {
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "add_variable",
            "name": "mcp_float",
            "value_type": "PVAL_FLOAT",
            "default_value": 1.0
        }])))
        .unwrap();
    assert_eq!(results[0].op, "add_variable");
    assert_eq!(results[0].path, "m_NamedValueLocals/0");
    // The op on the probe reproduces pet's own text (and therefore pet's key order: `build_entry`
    // builds the entry itself, unlike `add_function`, which sorts the caller's JSON) — the same
    // text the orchestrator read back from pet in `target/orch/live2/namedvalue.saved.vpcf`.
    assert_eq!(particle.write(), WITH_NAMEDVALUE);
    assert!(
        !particle.write().contains("m_IsPublic"),
        "pet omits `m_IsPublic` when it is true: {}",
        particle.write()
    );

    // The wire shape: `is_public` / `default_value` are optional, `Op::name` is the snake_case op.
    assert_eq!(
        op(json!({"op": "add_variable", "name": "v", "value_type": "PVAL_BOOL"})),
        Op::AddVariable {
            name: "v".to_string(),
            value_type: "PVAL_BOOL".to_string(),
            index: None,
            is_public: None,
            default_value: None,
        }
    );
    assert_eq!(
        op(json!({"op": "remove_variable", "index": 2})),
        Op::RemoveVariable { index: 2 }
    );
    assert_eq!(
        op(json!({"op": "add_variable", "name": "v", "value_type": "PVAL_BOOL"})).name(),
        "add_variable"
    );
    assert_eq!(op(json!({"op": "remove_variable", "index": 0})).name(), "remove_variable");
    assert!(serde_json::from_value::<Op>(json!({"op": "add_variable", "name": "v"})).is_err());
}

#[test]
fn add_variable_private_writes_is_public_false() {
    // pet writes `m_IsPublic` only when the value is `false`, and then between `m_Name` and
    // `m_DefaultConfig` (`target/orch/live2/namedvalue.numtype.txt`), never after `m_ValueType`.
    let mut particle = parse(PROBE);
    let results = particle
        .apply(&ops_value(json!([{
            "op": "add_variable",
            "name": "mcp_private",
            "value_type": "PVAL_INT",
            "index": 0,
            "is_public": false
        }])))
        .unwrap();
    assert_eq!(results[0].path, "m_NamedValueLocals/0");
    // The whole entry in one assertion: key order, and `default_value` unset is KV3 `null` for a
    // non-float type too (pet's own spelling: `namedvalue.noconfig.txt`).
    assert!(
        particle.write().contains(concat!(
            "\t\t{\r\n",
            "\t\t\tm_Name = \"mcp_private\"\r\n",
            "\t\t\tm_IsPublic = false\r\n",
            "\t\t\tm_DefaultConfig = \r\n",
            "\t\t\t{\r\n",
            "\t\t\t\tm_ConfigName = \"\"\r\n",
            "\t\t\t\tm_ConfigValue = null\r\n",
            "\t\t\t\tm_BoundValuePath = \"\"\r\n",
            "\t\t\t\tm_iAttachType = \"PATTACH_INVALID\"\r\n",
            "\t\t\t\tm_strEntityScope = \"\"\r\n",
            "\t\t\t\tm_strAttachmentName = \"\"\r\n",
            "\t\t\t}\r\n",
            "\t\t\tm_ValueType = \"PVAL_INT\"\r\n",
            "\t\t},\r\n"
        )),
        "{}",
        particle.write()
    );

    // ... and the same entry validates: `m_IsPublic = false` is fine, like `true`.
    let issues = particle.validate(&schema(), ValidateOptions::default());
    assert!(without_version_warning(&issues).is_empty(), "{:?}", messages(&issues));
}

#[test]
fn remove_variable_out_of_range_is_bad_op_and_unchanged() {
    let mut particle = parse(WITH_NAMEDVALUE);
    let before = particle.write();

    let error = particle
        .apply(&ops_value(json!([{"op": "remove_variable", "index": 1}])))
        .unwrap_err();
    assert!(
        error.to_string().contains("no entry at m_NamedValueLocals/1"),
        "{error}"
    );
    assert!(matches!(error, VpcfError::BadOp { index: 0, .. }), "{error:?}");
    assert_eq!(particle.write(), before, "a refused remove_variable changed the particle");

    // The in-range removal takes the entry out; the array key survives with `[  ]` (the writer's
    // shape for an emptied array, the same as `remove_function`).
    let results = particle
        .apply(&ops_value(json!([{"op": "remove_variable", "index": 0}])))
        .unwrap();
    assert_eq!(results[0].op, "remove_variable");
    assert_eq!(results[0].path, "m_NamedValueLocals/0");
    assert!(particle.list(Some("m_NamedValueLocals"), None).is_empty());
    assert!(
        particle.write().contains("\tm_NamedValueLocals = [  ]\r\n"),
        "{}",
        particle.write()
    );
}

#[test]
fn validate_rejects_duplicate_variable_names() {
    let mut particle = parse(WITH_NAMEDVALUE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_variable",
            "name": "mcp_float",
            "value_type": "PVAL_FLOAT"
        }])))
        .unwrap();

    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_NamedValueLocals/1");
    assert_eq!(issue.level, Level::Error, "{issue:?}");
    assert!(issue.msg.contains("duplicate m_Name \"mcp_float\""), "{issue:?}");
    assert!(issue.msg.contains("m_NamedValueLocals/0"), "{issue:?}");
    // Nothing else is wrong with the file, and strict validation refuses it on the duplicate.
    assert_eq!(without_version_warning(&issues).len(), 1, "{:?}", messages(&issues));
    assert!(particle.validate_strict(&schema, true).is_err());

    // An empty `m_Name` is refused too: the field names the variable.
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{"op": "add_variable", "name": "", "value_type": "PVAL_INT"}])))
        .unwrap();
    let issue = issue_at(
        &particle.validate(&schema, ValidateOptions::default()),
        "m_NamedValueLocals/0",
    );
    assert!(issue.msg.contains("m_Name is empty"), "{issue:?}");
}

#[test]
fn validate_rejects_a_value_type_without_pval_prefix() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{"op": "add_variable", "name": "v", "value_type": "FLOAT"}])))
        .unwrap();
    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_NamedValueLocals/0");
    assert_eq!(issue.level, Level::Error, "{issue:?}");
    assert!(
        issue.msg.contains("m_ValueType \"FLOAT\" does not start with \"PVAL_\""),
        "{issue:?}"
    );

    // A `PVAL_*` the snapshot does not know is a warning, never an error: the Pulse enum grows,
    // and `PVAL_VOID` is the class's own default.
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{"op": "add_variable", "name": "v", "value_type": "PVAL_VOID"}])))
        .unwrap();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_NamedValueLocals/0");
    assert_eq!(issue.level, Level::Warning, "{issue:?}");
    assert!(issue.msg.contains("PVAL_VOID"), "{issue:?}");
    assert!(
        issues.iter().all(|issue| !issue.level.is_error()),
        "{:?}",
        messages(&issues)
    );
    assert!(particle.validate_strict(&schema, true).is_ok());
}

#[test]
fn validate_rejects_a_non_string_value_type() {
    // Live: pet turns a non-string `m_ValueType` (the number 2) into `"PVAL_VOID"` silently
    // (`target/orch/live2/namedvalue.numtype.txt`), so pet can never have written a file whose
    // `m_ValueType` is not a string. The MCP server must say so instead of passing it on.
    let mut particle = parse(WITH_NAMEDVALUE);
    particle
        .apply(&ops_value(json!([{
            "op": "set_field",
            "array": "m_NamedValueLocals",
            "index": 0,
            "key": "m_ValueType",
            "value": 2
        }])))
        .unwrap();
    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_NamedValueLocals/0");
    assert_eq!(issue.level, Level::Error, "{issue:?}");
    assert!(
        issue.msg.contains("m_ValueType is not a string") && issue.msg.contains("got 2"),
        "{issue:?}"
    );
    assert_eq!(without_version_warning(&issues).len(), 1, "{:?}", messages(&issues));
    assert!(particle.validate_strict(&schema, true).is_err());
}

#[test]
fn validate_accepts_an_entry_without_default_config() {
    // Live: an entry without a `m_DefaultConfig` gets a full default config from pet, with
    // `m_ConfigValue = null` (`target/orch/live2/namedvalue.noconfig.txt`) — the file is not
    // broken, pet completes it, so the missing field is no issue at all.
    let mut particle = parse(WITH_NAMEDVALUE);
    particle
        .apply(&ops_value(json!([{
            "op": "remove_field",
            "array": "m_NamedValueLocals",
            "index": 0,
            "key": "m_DefaultConfig"
        }])))
        .unwrap();
    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    assert!(without_version_warning(&issues).is_empty(), "{:?}", messages(&issues));
    assert!(particle.validate_strict(&schema, true).is_ok());

    // Without a snapshot at all the missing config is still nothing to complain about (only a
    // present config cannot be checked then).
    assert!(vpcf::namedvalue::validate_entries(particle.root(), None).is_empty());
}

#[test]
fn validate_rejects_unknown_default_config_keys_with_the_test_schema() {
    let mut particle = parse(PROBE);
    particle
        .apply(&ops_value(json!([{
            "op": "add_variable",
            "name": "v",
            "value_type": "PVAL_FLOAT",
            "default_value": 1.0
        }])))
        .unwrap();
    assert!(
        particle.validate(&schema(), ValidateOptions::default())
            .iter()
            .all(|issue| !issue.level.is_error()),
        "the built entry must be clean"
    );

    // One unknown key inside `m_DefaultConfig`: the structural loop sees one valid field, the
    // named-value pass sees the key that `ParticleNamedValueConfiguration_t` does not have.
    particle
        .apply(&ops_value(json!([{
            "op": "set_field",
            "array": "m_NamedValueLocals",
            "index": 0,
            "key": "m_DefaultConfig",
            "value": {"m_ConfigName": "", "m_ConfigValue": 1.0, "m_Bogus": 1}
        }])))
        .unwrap();
    let issues = particle.validate(&schema(), ValidateOptions::default());
    let issue = issue_at(&issues, "m_NamedValueLocals/0");
    assert_eq!(issue.level, Level::Error, "{issue:?}");
    assert!(
        issue.msg.contains("unknown m_DefaultConfig key \"m_Bogus\"")
            && issue.msg.contains("ParticleNamedValueConfiguration_t"),
        "{issue:?}"
    );

    // A non-object `m_DefaultConfig` is refused as well (the field is a struct).
    particle
        .apply(&ops_value(json!([{
            "op": "set_field",
            "array": "m_NamedValueLocals",
            "index": 0,
            "key": "m_DefaultConfig",
            "value": 3
        }])))
        .unwrap();
    let issue = issue_at(
        &particle.validate(&schema(), ValidateOptions::default()),
        "m_NamedValueLocals/0",
    );
    assert!(issue.msg.contains("m_DefaultConfig is not an object"), "{issue:?}");
}

#[test]
fn validate_warns_when_the_snapshot_lacks_the_config_class() {
    let particle = parse(WITH_NAMEDVALUE);

    // The committed `research/pet-schema.json` is the live snapshot (2026-10-13, build 2000930) and
    // does carry `ParticleNamedValueConfiguration_t`, so pet's own `m_DefaultConfig` keys are
    // checked against it: the fixture entry is clean.
    let live = Schema::builtin().expect("the committed snapshot is usable");
    assert!(
        live.class("ParticleNamedValueSource_t").is_some(),
        "the live snapshot has the element class"
    );
    assert!(
        live.class("ParticleNamedValueConfiguration_t").is_some(),
        "the live snapshot must carry the config class the validator checks the keys against"
    );
    let issues = particle.validate(&live, ValidateOptions::default());
    assert!(without_version_warning(&issues).is_empty(), "{:?}", messages(&issues));
    assert!(particle.validate_strict(&live, true).is_ok());

    // A snapshot that (still) lacks the class cannot check the config keys. That must be visible (a
    // warning naming the fix), not silent — and never a guessed error. Built from the test snapshot
    // with the class removed, so the case stays covered whatever the committed file holds.
    let mut json: Value = serde_json::from_str(SCHEMA_TEST).expect("schema-test.json is JSON");
    assert!(
        json["classes"]
            .as_object_mut()
            .expect("schema-test.json has a classes object")
            .remove("ParticleNamedValueConfiguration_t")
            .is_some(),
        "schema-test.json must carry the config class for this test to remove it"
    );
    let stale = Schema::from_json(&json).unwrap_or_else(|error| panic!("{error}"));

    let issues = particle.validate(&stale, ValidateOptions::default());
    let warning = issue_at(&issues, "m_NamedValueLocals/0");
    assert_eq!(warning.level, Level::Warning, "{warning:?}");
    assert!(
        warning.msg.contains("snapshot has no ParticleNamedValueConfiguration_t"),
        "{warning:?}"
    );
    assert!(warning.msg.contains("particle_types {writeSnapshot:true}"), "{warning:?}");
    // The missing class is what a refresh fixes, not a defect of the file: no errors, so the file
    // still validates.
    assert!(
        issues.iter().all(|issue| !issue.level.is_error()),
        "{:?}",
        messages(&issues)
    );
    assert!(particle.validate_strict(&stale, true).is_ok());

    // `schema: None` (no snapshot at all) takes the same path: warn, never guess.
    let bare = vpcf::namedvalue::validate_entries(particle.root(), None);
    let warning = issue_at(&bare, "m_NamedValueLocals/0");
    assert_eq!(warning.level, Level::Warning, "{warning:?}");
    assert!(
        warning.msg.contains("snapshot has no ParticleNamedValueConfiguration_t"),
        "{warning:?}"
    );
}

#[test]
fn deep_nesting_is_a_parse_error_not_a_crash() {
    // A KV3 body nested past the parser's depth cap must be a `VpcfError::Parse`, never a stack
    // overflow: `resourcecompiler`/pet recurse over whatever `cs2_kv3` accepts, and the bridge's
    // lexer gate (`KV3_CHECK_DEPTH_CAP`) mirrors this cap. The cap lands with WP-K (`cs2-kv3`,
    // `cs2_kv3::MAX_DEPTH`); until that is merged into this branch the parser accepts the text and
    // this test fails — it is written now on purpose (see the WP-V report).
    let depth = |levels: usize| {
        format!(
            "{}\n{{\n\t_class = \"{ROOT_CLASS}\"\n\tm_mcpDeep = {}{}\n}}\n",
            vpcf::HEADER,
            "[".repeat(levels),
            "]".repeat(levels)
        )
    };
    // A shallow nest is ordinary KV3: the cap refuses depth, not arrays.
    assert!(Particle::parse(&depth(64)).is_ok(), "64 nested arrays must still parse");
    let error = match Particle::parse(&depth(300)) {
        Ok(_) => panic!("300 nested arrays parsed: the cs2-kv3 depth cap (WP-K) is missing"),
        Err(error) => error,
    };
    assert!(matches!(error, VpcfError::Parse(_)), "{error:?}");
}

#[test]
fn validate_fails_closed_when_the_schema_lacks_a_class_it_needs() {
    // No `ParticleChildrenInfo_t`: the type 7 entries cannot be checked, so the file is refused
    // rather than silently passed.
    let mut json: Value = serde_json::from_str(SCHEMA_TEST).unwrap();
    json["classes"].as_object_mut().unwrap().remove("ParticleChildrenInfo_t");
    let schema = Schema::from_json(&json).unwrap();
    let issues = parse(WITH_CHILDREN).validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "m_Children/0");
    assert!(
        issue.msg.contains("schema has no class \"ParticleChildrenInfo_t\""),
        "{issue:?}"
    );

    // A schema without the root class cannot check the root fields either.
    let mut json: Value = serde_json::from_str(SCHEMA_TEST).unwrap();
    json["classes"].as_object_mut().unwrap().remove(ROOT_CLASS);
    let schema = Schema::from_json(&json).unwrap();
    let issues = parse(PROBE).validate(&schema, ValidateOptions::default());
    let issue = issue_at(&issues, "/");
    assert!(issue.msg.contains("schema has no class"), "{issue:?}");
}

#[test]
fn validate_structure_reports_malformed_arrays_and_headers() {
    let particle = vpcf("\tm_Operators = 3\n");
    let issues = particle.validate_structure();
    let issue = issue_at(&issues, "m_Operators");
    assert!(issue.msg.contains("is not an array of objects"), "{issue:?}");

    let particle = vpcf("\tm_Operators = [ 1, ]\n");
    let issue = issue_at(&particle.validate_structure(), "m_Operators/0");
    assert!(issue.msg.contains("entry is not an object"), "{issue:?}");

    let particle = vpcf("\tm_Operators = [ { m_fDrag = 1.0 }, ]\n");
    let issue = issue_at(&particle.validate_structure(), "m_Operators/0");
    assert!(issue.msg.contains("_class is missing"), "{issue:?}");

    // The structural arrays do not need a `_class`; the only issue left is the behavior-version
    // warning (this hand-built file predates the field too).
    let particle = vpcf("\tm_Children = [ { m_ChildRef = resource:\"particles/x.vpcf\" }, ]\n");
    let issues = particle.validate_structure();
    assert!(without_version_warning(&issues).is_empty(), "{issues:?}");
    assert_eq!(issues.len(), 1);

    // A non-vpcf header is a structural error.
    let text = PROBE.replace("format:vpcf66", "format:modeldoc36");
    let issues = Particle::parse(&text).unwrap().validate_structure();
    let issue = issue_at(&issues, "/");
    assert!(issue.msg.contains("is not \"vpcf66\""), "{issue:?}");
}

// ---------------------------------------------------------------- schema

#[test]
fn schema_placeholder_is_rejected() {
    let placeholder: Value = serde_json::from_str(PLACEHOLDER).unwrap();
    assert_eq!(placeholder["placeholder"], json!(true));
    assert!(matches!(
        Schema::from_json(&placeholder),
        Err(VpcfError::SchemaPlaceholder)
    ));
}

/// The committed snapshot is the live one (`particle_types {writeSnapshot:true}` against pet,
/// build 2000930, 2026-10-13): the registry and the root/structural classes are all present.
#[test]
fn the_built_in_snapshot_is_the_live_one() {
    let schema = Schema::builtin().expect("the committed snapshot is usable");
    assert_eq!(schema.build.as_deref(), Some("2000930"));
    assert_eq!(schema.root_class, ROOT_CLASS);
    assert_eq!(schema.arrays.len(), 9);
    assert_eq!(schema.functions.len(), 376);
    let probe = Particle::parse(PROBE).unwrap();
    let issues = probe.validate(&schema, ValidateOptions { allow_obsolete: true });
    assert!(issues.iter().all(|issue| !issue.level.is_error()), "{issues:?}");
}

#[test]
fn schema_from_json_rejects_malformed_documents() {
    for (json, expected) in [
        (json!("not an object"), "must be a JSON object"),
        (json!({"functions": {}}), "functions must be an array"),
        (json!({"arrays": {}}), "arrays must be an array"),
        (json!({"classes": []}), "classes must be an object"),
        (json!({"functions": [{"type": 1}]}), "className is required"),
        (json!({"functions": [{"type": 1, "className": "X", "obsolete": "yes"}]}), "obsolete must be a bool"),
        (json!({"arrays": [{"type": "x", "key": "k"}]}), "type must be a non-negative integer"),
        (json!({"arrays": [{"type": 0}]}), "key is required"),
        (json!({"classes": {"X": {"fields": [{"name": "f", "type": 7}]}}}), "expected a string or null"),
        (json!({"classes": {"X": {"bases": [1]}}}), "bases entries must be strings"),
        (json!({"rootClass": 3}), "rootClass must be a non-empty string"),
        (json!({"build": 3}), "build must be a string or null"),
    ] {
        let error = Schema::from_json(&json).unwrap_err();
        assert!(matches!(error, VpcfError::Schema(_)), "{json}: {error:?}");
        assert!(error.to_string().contains(expected), "{json}: {error}");
    }
}

#[test]
fn schema_load_and_known_schema_prefer_the_file_over_the_builtin() {
    let dir = scratch("vpcf-schema-known");
    let missing = dir.join("pet-schema.json");
    let (result, source) = known_schema(&missing);
    assert_eq!(result.unwrap().build.as_deref(), Some("2000930"), "{source}");
    assert_eq!(source, "built-in (build 2000930)");
    assert!(matches!(
        Schema::load(&missing),
        Err(VpcfError::Io(_))
    ));

    let file = dir.join("pet-schema.json");
    std::fs::write(&file, SCHEMA_TEST).unwrap();
    let (result, source) = known_schema(&file);
    assert_eq!(source, file.display().to_string());
    let schema = result.unwrap();
    assert_eq!(schema.build.as_deref(), Some("test"));
    assert_eq!(schema.root_class, ROOT_CLASS);
    assert_eq!(schema.arrays.len(), 9);
    assert_eq!(schema.functions.len(), 7);

    // A snapshot in the way that does not exist is the only load error beside the placeholder
    // that falls back to the built-in; any other error (JSON syntax, wrong shape) is returned,
    // naming the file in `source`, so a corrupt snapshot is never silently "validated" against
    // the built-in.
    std::fs::write(&file, "not json").unwrap();
    let (result, source) = known_schema(&file);
    assert!(matches!(result, Err(VpcfError::Schema(_))), "{result:?}");
    assert_eq!(source, file.display().to_string());

    // The placeholder still falls back (it is "no newer snapshot here", not a corrupt one).
    std::fs::write(&file, PLACEHOLDER).unwrap();
    let (result, source) = known_schema(&file);
    assert_eq!(result.unwrap().build.as_deref(), Some("2000930"));
    assert_eq!(source, "built-in (build 2000930)");
}

#[test]
fn template_classes_are_in_the_schema_snapshot() {
    let schema = schema();
    for class in TEMPLATE_CLASSES {
        assert!(schema.class(class).is_some(), "{class} is not in the test schema");
    }
    assert_eq!(
        schema.registration_types("C_OP_ContinuousEmitter"),
        vec![1]
    );
    assert_eq!(
        schema.registration_types("C_INIT_CreateWithinSphereTransform"),
        vec![2]
    );
    assert_eq!(schema.registration_types("C_OP_BasicMovement"), vec![3]);
    assert_eq!(schema.registration_types("C_OP_RenderSprites"), vec![6]);
}

// ---------------------------------------------------------------- templates

#[test]
fn template_minimal_is_the_probe_fixture_with_the_version_line() {
    let particle = template("minimal", &json!({})).unwrap();
    // The template output is the probe fixture plus exactly the `m_nBehaviorVersion = 12` line.
    assert_eq!(
        particle.write(),
        probe_with_behavior_version("12"),
        "the template is not the probe fixture with the version line"
    );
    assert_eq!(particle.doc.header.format, "vpcf66");
    assert_eq!(
        particle.doc.header.raw,
        PROBE.lines().next().unwrap(),
        "the template header is not the fixture's"
    );
    assert_eq!(
        classes(&particle),
        [
            "C_OP_ContinuousEmitter",
            "C_INIT_CreateWithinSphereTransform",
            "C_OP_BasicMovement",
            "C_OP_Decay",
            "C_OP_RenderSprites",
        ]
    );
    let schema = schema();
    assert!(particle.validate(&schema, ValidateOptions::default()).is_empty());
    assert!(particle.validate_strict(&schema, true).is_ok());
    assert_eq!(TEMPLATE_CLASSES.len(), 6);
    assert_eq!(TEMPLATE_CLASSES[0], ROOT_CLASS);
    // The default is `32`, and `params: {}` is the same as the defaults.
    assert_eq!(
        template("minimal", &json!({"maxParticles": 32, "renderer": "C_OP_RenderSprites"}))
            .unwrap()
            .write(),
        probe_with_behavior_version("12")
    );
}

#[test]
fn template_minimal_takes_max_particles_and_a_renderer() {
    let particle = template(
        "minimal",
        &json!({"maxParticles": 512, "renderer": "C_OP_RenderSprites"}),
    )
    .unwrap();
    assert!(particle.write().contains("\tm_nMaxParticles = 512\r\n"), "{}", particle.write());
    assert_eq!(
        particle.list(Some("m_Renderers"), None)[0].class.as_deref(),
        Some("C_OP_RenderSprites")
    );

    let particle = template("minimal", &json!({"renderer": "C_OP_RenderTrails"})).unwrap();
    assert_eq!(
        particle.list(Some("m_Renderers"), None)[0].class.as_deref(),
        Some("C_OP_RenderTrails")
    );
    // The renderer is only a class name here; the schema is what says whether it exists.
    let issues = particle.validate(&schema(), ValidateOptions::default());
    assert!(issues.iter().any(|issue| issue.msg.contains("C_OP_RenderTrails")), "{issues:?}");
}

#[test]
fn template_refuses_bad_arguments() {
    for (kind, params, expected) in [
        ("prop", json!({}), "unknown template \"prop\""),
        ("minimal", json!(null), "params must be an object"),
        ("minimal", json!({"maxParticles": -1}), "must be a non-negative integer"),
        ("minimal", json!({"maxParticles": "many"}), "must be a non-negative integer"),
        ("minimal", json!({"maxParticles": 1.5}), "must be a non-negative integer"),
        ("minimal", json!({"renderer": ""}), "params.renderer must be a non-empty string"),
        ("minimal", json!({"renderer": 3}), "params.renderer must be a non-empty string"),
    ] {
        let error = template(kind, &params).unwrap_err();
        assert!(matches!(error, VpcfError::Invalid(_)), "{error:?}");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

// ---------------------------------------------------------------- behavior version

// The root-field check shared by Particle::validate_structure and the schema Particle::validate:
// a missing or stale `m_nBehaviorVersion` is a warning (pet shows the red "old behavior version"
// banner, fact A; `resourcecompiler` still compiles the file), a non-integer is an error,
// >= the current version is accepted silently.
#[test]
fn template_sets_current_behavior_version() {
    let particle = template("minimal", &json!({})).unwrap();
    let written = particle.write();
    // `m_nBehaviorVersion = 12`, placed directly after `_class`.
    assert!(
        written.contains(
            "\t_class = \"CParticleSystemDefinition\"\r\n\tm_nBehaviorVersion = 12\r\n"
        ),
        "{written}"
    );
    assert_eq!(
        written,
        probe_with_behavior_version("12"),
        "the template writes the probe fixture plus exactly the version line"
    );
    // It parses back, and the structural pass says nothing about the field.
    let reparsed = parse(&written);
    let issues = reparsed.validate_structure();
    assert!(issues.is_empty(), "{:?}", messages(&issues));
}

#[test]
fn missing_behavior_version_is_a_warning_not_an_error() {
    // The probe fixture is the live-compiled reference file: it has no `m_nBehaviorVersion`, so
    // pet treats it as 0 (live fact A) — exactly one warning, zero errors.
    let particle = parse(PROBE);
    let issues = particle.validate_structure();
    let warning = issue_at(&issues, "m_nBehaviorVersion");
    assert_eq!(warning.level, Level::Warning);
    assert!(warning.msg.contains("missing"), "{warning:?}");
    assert!(warning.msg.contains("12"), "{warning:?}");
    assert!(warning.msg.contains("old behavior version"), "{warning:?}");
    assert_eq!(
        issues.iter().filter(|issue| issue.level.is_error()).count(),
        0,
        "a missing behavior version must not be an error: {issues:?}"
    );
    // The schema pass reports the same single warning (the field is exempt from the root-field
    // list, which the hand-made test snapshot does not carry).
    let issues = particle.validate(&schema(), ValidateOptions::default());
    assert_eq!(issue_at(&issues, "m_nBehaviorVersion").level, Level::Warning);
    assert!(without_version_warning(&issues).is_empty(), "{:?}", messages(&issues));
    assert!(particle.validate_strict(&schema(), true).is_ok());
    assert!(particle.validate_strict(&schema(), false).is_ok());
}

#[test]
fn old_behavior_version_is_a_warning() {
    let particle = parse(&probe_with_behavior_version("3"));
    let issues = particle.validate_structure();
    let warning = issue_at(&issues, "m_nBehaviorVersion");
    assert_eq!(warning.level, Level::Warning);
    assert!(warning.msg.contains("3"), "{warning:?}");
    assert!(warning.msg.contains("12"), "{warning:?}");
    assert!(warning.msg.contains("old behavior version"), "{warning:?}");
    assert_eq!(
        issues.iter().filter(|issue| issue.level.is_error()).count(),
        0,
        "{issues:?}"
    );
    // The schema pass again: same warning, no unknown-root-field noise.
    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    assert_eq!(messages(&issues).len(), 1, "{issues:?}");
    assert_eq!(issue_at(&issues, "m_nBehaviorVersion").level, Level::Warning);

    // Greater than the current version is accepted silently (no issue on the path at all).
    let newer = parse(&probe_with_behavior_version("13"));
    assert_eq!(newer.validate_structure(), Vec::new());
    assert_eq!(newer.validate(&schema, ValidateOptions::default()), Vec::new());
}

#[test]
fn non_integer_behavior_version_is_an_error() {
    let particle = parse(&probe_with_behavior_version("\"x\""));
    let issues = particle.validate_structure();
    let issue = issue_at(&issues, "m_nBehaviorVersion");
    assert_eq!(issue.level, Level::Error, "{issue:?}");
    assert!(issue.msg.contains("\"x\""), "{issue:?}");
    assert_eq!(
        issues.iter().filter(|issue| issue.level.is_error()).count(),
        1,
        "{issues:?}"
    );
    // The schema pass reports the same error, and strict refuses the file on it.
    let schema = schema();
    let issues = particle.validate(&schema, ValidateOptions::default());
    assert_eq!(issue_at(&issues, "m_nBehaviorVersion").level, Level::Error);
    let error = particle.validate_strict(&schema, true).unwrap_err();
    assert!(error.to_string().contains("m_nBehaviorVersion"), "{error}");
}

// ---------------------------------------------------------------- content paths

fn content_tree(tag: &str) -> PathBuf {
    let root = scratch(tag);
    for relative in [
        "content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf",
        "content/csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl",
        "content/csgo/particles/breakable.vpcf",
    ] {
        let file = root.join(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, PROBE).unwrap();
    }
    root
}

fn content_paths(root: &Path) -> (ContentPaths, cs2_mcp_common::pathmap::PathMap) {
    let map =
        cs2_mcp_common::pathmap::PathMap::parse(&format!("z:\\app={}", root.display())).unwrap();
    (ContentPaths::new(root, "mcp_test"), map)
}

#[test]
fn resolve_read_ext_accepts_all_three_forms_for_a_vpcf() {
    let root = content_tree("vpcf-paths-read");
    let (paths, map) = content_paths(&root);
    let expected = root.join("content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf");
    for form in [
        expected.display().to_string(),
        "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf".to_string(),
        "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf".to_string(),
    ] {
        assert_eq!(paths.resolve_read_ext(&form, &map, "vpcf").unwrap(), expected, "{form}");
    }
    // Core content is readable.
    assert!(paths.resolve_read_ext("csgo/particles/breakable.vpcf", &map, "vpcf").is_ok());
    // The old entry points still mean `.vmdl`.
    assert!(paths
        .resolve_read_ext("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl", &map, "vmdl")
        .is_ok());
}

#[test]
fn resolve_ext_refuses_the_wrong_extension_dot_dot_and_core_writes() {
    let root = content_tree("vpcf-paths-refuse");
    let (paths, map) = content_paths(&root);
    let probe = "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf";

    // Wrong extension: a `.vmdl` where the particle route wants a `.vpcf`, and the reverse.
    let error = paths
        .resolve_read_ext("csgo_addons/mcp_test/models/mcp_probe/glass_probe.vmdl", &map, "vpcf")
        .unwrap_err();
    assert!(matches!(error, VmdlError::Path(_)), "{error:?}");
    assert!(error.to_string().contains("is not a .vpcf file"), "{error}");
    let error = paths.resolve_read(probe, &map).unwrap_err();
    assert!(error.to_string().contains("is not a .vmdl file"), "{error}");

    // `..`, an empty path and an unmapped Windows drive are all refused, for both ext values.
    for ext in ["vpcf", "vmdl"] {
        for (input, expected) in [
            ("csgo_addons/../../etc/passwd.vpcf", "contains `..`"),
            ("..\\..\\secrets.vpcf", "contains `..`"),
            ("", "path is empty"),
            ("Q:\\app\\content\\x.vpcf", "has no host mapping"),
        ] {
            let error = paths.resolve_read_ext(input, &map, ext).unwrap_err();
            assert!(matches!(error, VmdlError::Path(_)), "{ext} {input}: {error:?}");
            assert!(error.to_string().contains(expected), "{ext} {input}: {error}");
        }
    }

    // Writes land in the active addon only; core content and another addon are refused.
    let expected = root.join("content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf");
    assert_eq!(paths.resolve_write_ext(probe, &map, "vpcf").unwrap(), expected);
    assert_eq!(
        paths
            .resolve_write_ext(
                "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf",
                &map,
                "vpcf"
            )
            .unwrap(),
        expected
    );
    // A directory that does not exist yet is fine (`particle_create` makes it).
    assert_eq!(
        paths
            .resolve_write_ext("csgo_addons/mcp_test/particles/mcp_live/run_1.vpcf", &map, "vpcf")
            .unwrap(),
        root.join("content/csgo_addons/mcp_test/particles/mcp_live/run_1.vpcf")
    );
    for input in [
        "csgo/particles/breakable.vpcf".to_string(),
        "csgo_addons/other_addon/particles/x.vpcf".to_string(),
        root.join("game/csgo_addons/mcp_test/particles/x.vpcf").display().to_string(),
    ] {
        let error = paths.resolve_write_ext(&input, &map, "vpcf").unwrap_err();
        assert!(matches!(error, VmdlError::Path(_)), "{input}: {error:?}");
        assert!(error.to_string().contains("only files of the active addon"), "{input}: {error}");
    }
    // The `.vmdl` route keeps its wording (asserted in `tests/vmdl.rs`, repeated for the refactor).
    let error = paths
        .resolve_write("csgo_addons/other_addon/models/x.vmdl", &map)
        .unwrap_err();
    assert!(error.to_string().contains("only models of the active addon"), "{error}");
}

/// A project root reached through a symlink (live 2026-10-13: a staging `--project-root` whose
/// `workshop` is a link). `resolve_parent` canonicalizes a not-yet-existing file's parent, so the
/// containment check must compare against the canonical addon root; before the fix every new-file
/// write was refused as `WriteOutsideAddon`.
#[cfg(unix)]
#[test]
fn a_symlinked_app_root_still_accepts_new_files_in_the_addon() {
    let real = content_tree("vpcf-paths-symlink-real");
    let link = scratch("paths-symlink-link").join("app");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let paths = ContentPaths::new(&link, "mcp_test");
    let map = cs2_mcp_common::pathmap::PathMap::parse(&format!("z:\\app={}", link.display())).unwrap();

    let new_file = "csgo_addons/mcp_test/particles/mcp_live/new_1.vpcf";
    let host = paths.resolve_write_ext(new_file, &map, "vpcf").unwrap();
    assert_eq!(host, real.join("content/csgo_addons/mcp_test/particles/mcp_live/new_1.vpcf"));
    assert_eq!(paths.addon_rel(&host).as_deref(), Some("particles/mcp_live/new_1.vpcf"));
    // An existing file resolves the same way.
    let probe = paths
        .resolve_write_ext("csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf", &map, "vpcf")
        .unwrap();
    assert_eq!(paths.addon_rel(&probe).as_deref(), Some("particles/mcp_probe/probe.vpcf"));
    // The negative half: through the link, another addon is still refused.
    let error = paths
        .resolve_write_ext("csgo_addons/other_addon/particles/x.vpcf", &map, "vpcf")
        .unwrap_err();
    assert!(error.to_string().contains("only files of the active addon"), "{error}");
}

#[test]
fn derived_paths_work_for_a_vpcf() {
    let root = content_tree("vpcf-paths-derived");
    let (paths, _map) = content_paths(&root);
    let host = root.join("content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf");
    assert_eq!(
        paths.addon_rel(&host).as_deref(),
        Some("particles/mcp_probe/probe.vpcf")
    );
    assert_eq!(
        paths.content_rel(&host).as_deref(),
        Some("csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf")
    );
    assert_eq!(
        paths.container_path(&host).as_deref(),
        Some("/app/content/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf")
    );
    // `compiled_output` already works for `.vpcf` -> `.vpcf_c`.
    assert_eq!(
        paths.compiled_output(&host),
        Some(root.join("game/csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf_c"))
    );
    assert!(paths.in_addon(&host));
}
