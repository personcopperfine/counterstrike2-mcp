//! `particle_file` tool tests (WP4): read/list/validate/edit/create/compile against a temp content
//! tree (under `CARGO_TARGET_TMPDIR`, never /tmp) and a fake podman script.
//!
//! Only the six file-route tools are registered here ([`particle_file::register`]): the live half
//! is `particle_live.rs` (WP5) and the combined `PET_TOOL_ORDER` is written at merge time, so
//! `build_server` does not know these tools yet — [`server`] builds the registry itself.
//!
//! `particle_compile` runs through the fake podman harness copied from `tests/model_file.rs`
//! (model/report symmetry): `ps` reports a container, `exec` maps `-i /app/content/<rel>` to
//! `<app>/game/<rel>_c`, touches it (or not) and prints the `OK: … compiled …` summary the tool
//! parses. No container, no game and no bridge is involved anywhere in this file.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mcp::{McpServer, ServerIdentity};
use cs2_mcp_common::tools::{CallFailure, Mutability, Registry, ToolError};
use hammer_mcp::state::HammerState;
use hammer_mcp::vpcf::TEMPLATE_CLASSES;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod common;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The kv3 header of a `.vpcf` (the GUIDs pet passes to LoadKV3/SaveKV3).
const HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->";
/// `fixtures/vpcf/schema-test.json`: a hand-made, non-placeholder snapshot (`build:"test"`).
const SCHEMA_TEST: &str = include_str!("../fixtures/vpcf/schema-test.json");
/// The live-compiled probe fixture; the template output differs from it by exactly the
/// `m_nBehaviorVersion` line (see `probe_created_text`).
const PROBE_FIXTURE: &str = include_str!("../fixtures/vpcf/probe.vpcf");

/// `probe.vpcf` with the template's `m_nBehaviorVersion = 12` line inserted directly after
/// `_class`, constructed from the fixture text (the fixture itself is the live-compiled
/// reference file and stays without the field).
fn probe_created_text() -> String {
    let line = "\t_class = \"CParticleSystemDefinition\"\r\n";
    assert!(PROBE_FIXTURE.contains(line), "the probe fixture changed");
    PROBE_FIXTURE.replacen(line, &format!("{line}\tm_nBehaviorVersion = 12\r\n"), 1)
}

/// A temp project tree laid out like the real one: `--project-root` at `root`, app dir at
/// `workshop/run/app`, addon content under its `content/csgo_addons/mcp_test`.
struct Tree {
    root: PathBuf,
    app: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "particle-file-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("tmp tree");
        let app = root.join("workshop/run/app");
        Tree { root, app }
    }

    fn addon(&self) -> PathBuf {
        self.app.join("content/csgo_addons/mcp_test")
    }

    /// Copy a fixture from `hammer-mcp/fixtures/vpcf/` into the addon at addon-relative `rel`.
    fn install(&self, fixture: &str, rel: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("install dir");
        let source =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/vpcf").join(fixture);
        std::fs::copy(&source, &target).expect("install fixture");
        target
    }

    /// Write `text` under the addon at `rel` (creating parents).
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let target = self.addon().join(rel);
        std::fs::create_dir_all(target.parent().expect("rel has a parent")).expect("write dir");
        std::fs::write(&target, text).expect("write file");
        target
    }

    /// Write the particle schema snapshot where the tools look first (`<project>/hammer-mcp/research/...`).
    fn write_snapshot(&self, text: &str) -> PathBuf {
        let dir = self.root.join("hammer-mcp/research");
        std::fs::create_dir_all(&dir).expect("snapshot dir");
        let path = dir.join("pet-schema.json");
        std::fs::write(&path, text).expect("snapshot file");
        path
    }

    /// The committed test snapshot, unchanged.
    fn install_snapshot(&self) -> PathBuf {
        self.write_snapshot(SCHEMA_TEST)
    }

    /// The path `particle_compile` at addon-relative `rel` must report as `outputPath`
    /// (`<app>/game/csgo_addons/mcp_test/<rel>_c`, the rule `ContentPaths::compiled_output` uses).
    fn compile_output(&self, rel: &str) -> PathBuf {
        self.app
            .join("game")
            .join(format!("csgo_addons/mcp_test/{rel}_c"))
    }

    /// Every entry next to `path`'s file name (`probe.vpcf`, `probe.vpcf.bak-…`, …), sorted
    /// (`read_dir` order is arbitrary).
    fn siblings(&self, path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .expect("siblings")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A fake `podman` binary under the tree (mode 0755). Kinds:
/// * `ok` — `ps` reports a container; `exec` maps `-i /app/content/<rel>` to `<app>/game/<rel>_c`,
///   touches it and prints the OK summary;
/// * `fail` — prints a `[FAIL]` line plus a `FATAL ERROR` and does NOT touch the output;
/// * `skipped` — prints `OK: 0 compiled, 0 failed, 1 skipped` and does NOT touch the output;
/// * `no-container` — `ps` reports nothing (no container carries the label).
fn fake_podman(tree: &Tree, kind: &str) -> PathBuf {
    let bin_dir = tree.root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin dir");
    let body = match kind {
        "ok" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    rel="${last##* -i }"
    case "$rel" in
      /app/content/*) rel="${rel#/app/content/}" ;;
      *) echo "fake podman: unexpected command: $last" >&2
         exit 3 ;;
    esac
    out="$APP/game/${rel}_c"
    mkdir -p "$(dirname "$out")"
    touch "$out"
    echo "Compiling $rel"
    echo "(compiler noise goes to stderr)" >&2
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
  *)
    echo "fake podman: unsupported verb $1" >&2
    exit 2
    ;;
esac
"#,
        "fail" => r#"#!/bin/bash
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "[FAIL] dependency failed: particles/missing/missing.vpcf"
    echo "FATAL ERROR: unable to load gameinfo.gi (bad -game path)" >&2
    exit 1
    ;;
esac
"#,
        // like `ok`, but for several `-i` in one command; logs every exec's command line to
        // `__ROOT__/exec.log` (one line per exec) so a test can see what ran, and how often
        "multi" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    echo "$last" >> '__ROOT__/exec.log'
    read -ra toks <<< "$last"
    n=0
    for ((i = 0; i < ${#toks[@]}; i++)); do
      if [ "${toks[$i]}" = "-i" ]; then
        rel="${toks[$((i + 1))]}"
        rel="${rel#/app/content/}"
        out="$APP/game/${rel}_c"
        mkdir -p "$(dirname "$out")"
        touch "$out"
        n=$((n + 1))
      fi
    done
    echo "OK: $n compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
esac
"#,
        "multi-more" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    echo "$last" >> '__ROOT__/exec.log'
    read -ra toks <<< "$last"
    n=0
    for ((i = 0; i < ${#toks[@]}; i++)); do
      if [ "${toks[$i]}" = "-i" ]; then
        rel="${toks[$((i + 1))]}"
        rel="${rel#/app/content/}"
        out="$APP/game/${rel}_c"
        mkdir -p "$(dirname "$out")"
        touch "$out"
        n=$((n + 1))
      fi
    done
    echo "OK: $((n + 1)) compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
esac
"#,
        "multi-fail" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    echo "$last" >> '__ROOT__/exec.log'
    read -ra toks <<< "$last"
    n=0
    for ((i = 0; i < ${#toks[@]}; i++)); do
      if [ "${toks[$i]}" = "-i" ]; then
        rel="${toks[$((i + 1))]}"
        rel="${rel#/app/content/}"
        out="$APP/game/${rel}_c"
        mkdir -p "$(dirname "$out")"
        touch "$out"; case "$rel" in *bad.vpcf) echo "[FAIL] dependency failed: $rel" ;; esac
        n=$((n + 1))
      fi
    done
    echo "OK: $n compiled, 1 failed, 0 skipped, 0m:01s"
    ;;
esac
"#,
        "multi-short" => r#"#!/bin/bash
APP='__APP__'
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    last="${!#}"
    echo "$last" >> '__ROOT__/exec.log'
    read -ra toks <<< "$last"
    n=0
    for ((i = 0; i < ${#toks[@]}; i++)); do
      if [ "${toks[$i]}" = "-i" ]; then
        rel="${toks[$((i + 1))]}"
        rel="${rel#/app/content/}"
        out="$APP/game/${rel}_c"
        mkdir -p "$(dirname "$out")"
        touch "$out"
        n=$((n + 1))
      fi
    done
    echo "OK: 1 compiled, 0 failed, 0 skipped, 0m:01s"
    ;;
esac
"#,
        "skipped" => r#"#!/bin/bash
case "$1" in
  ps)
    printf '[{"Names":["cs2tools"],"State":"running"}]'
    exit 0
    ;;
  exec)
    echo "OK: 0 compiled, 0 failed, 1 skipped, 0m:00s"
    ;;
esac
"#,
        "no-container" => r#"#!/bin/bash
case "$1" in
  ps) echo '[]' ;;
  *) echo "fake podman: unsupported verb $1" >&2; exit 2 ;;
esac
"#,
        other => panic!("unknown fake podman kind {other}"),
    }
    .replace("__APP__", &tree.app.display().to_string())
    .replace("__ROOT__", &tree.root.display().to_string());
    let script = bin_dir.join(format!("podman-{kind}"));
    std::fs::write(&script, body).expect("fake podman script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn config(tree: &Tree, podman: Option<String>, tool_budget_ms: u64) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let budget = tool_budget_ms.to_string();
    let root = tree.root.display().to_string();
    let map = format!("z:\\app={}", tree.app.display());
    let mut args: Vec<String> = vec![
        "--deadline-ms".into(),
        "5000".into(),
        "--tool-budget-ms".into(),
        budget,
        "--project-root".into(),
        root,
        "--path-map".into(),
        map,
    ];
    if let Some(podman) = podman {
        args.extend(["--podman".into(), podman]);
    }
    let given: Vec<&str> = args.iter().map(String::as_str).collect();
    let argv: Vec<&str> = [vec!["t"], common::hermetic(&given)].concat();
    W::parse_from(argv).c
}

/// A server with exactly the six file-route particle tools registered.
fn compile_server(tree: &Tree, podman: Option<String>, tool_budget_ms: u64) -> McpServer<HammerState> {
    let cfg = config(tree, podman, tool_budget_ms);
    let state = HammerState::from_config(&cfg).expect("state");
    let mut registry = Registry::new();
    hammer_mcp::tools::particle_file::register(&mut registry);
    McpServer::new(
        registry,
        state,
        ServerIdentity {
            name: "hammer-mcp",
            version: "test",
            instructions: String::new(),
        },
        cfg.tool_budget_ms,
        cfg.deadline_ms,
    )
}

fn plain_server(tree: &Tree) -> McpServer<HammerState> {
    compile_server(tree, None, 45_000)
}

async fn call(
    server: &McpServer<HammerState>,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    match server.call(name, args).await {
        Ok(out) => Ok(out.json),
        Err(CallFailure::Tool(e)) => Err(e),
        Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
    }
}

const PROBE_REL: &str = "particles/mcp_probe/probe.vpcf";
/// Addon-relative form of [`PROBE_REL`] (what the tools accept).
const PROBE_ARG: &str = "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf";
/// Windows form of [`PROBE_ARG`].
const PROBE_WIN: &str =
    "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf";

/// A `.vpcf` with just a root class, `m_nMaxParticles` and the given `m_Operators` entries.
fn vpcf_with(operator_entries: &str) -> String {
    format!(
        "{HEADER}\n{{\n\t_class = \"CParticleSystemDefinition\"\n\tm_nMaxParticles = 8\n\t\
         m_Operators = \n\t[\n{operator_entries}\t]\n}}"
    )
}

/// One `m_Operators` entry: `_class` first, then the extra fields.
fn entry(class: &str, fields: &[(&str, &str)]) -> String {
    let mut body = format!("\t\t{{\n\t\t\t_class = \"{class}\"\n");
    for (key, value) in fields {
        body.push_str(&format!("\t\t\t{key} = {value}\n"));
    }
    body.push_str("\t\t},\n");
    body
}

// -------------------------------------------------------------------------------------------
// registration
// -------------------------------------------------------------------------------------------

#[test]
fn register_exposes_the_six_file_tools_with_the_right_policies() {
    let mut registry = Registry::new();
    hammer_mcp::tools::particle_file::register(&mut registry);
    assert_eq!(
        registry.names(),
        hammer_mcp::tools::particle_file::PARTICLE_FILE_TOOL_ORDER.to_vec(),
        "WP4's half of PET_TOOL_ORDER, in order"
    );
    for (name, mutability) in [
        ("particle_read", Mutability::ReadOnly),
        ("particle_list", Mutability::ReadOnly),
        ("particle_validate", Mutability::ReadOnly),
        ("particle_edit", Mutability::Writer),
        ("particle_create", Mutability::Writer),
        ("particle_compile", Mutability::Writer),
    ] {
        let spec = registry.spec(name).unwrap_or_else(|| panic!("{name} registered"));
        assert_eq!(spec.mutability, mutability, "{name}");
        assert!(spec.bridge_methods.is_empty(), "{name} is a file-route tool");
    }
}

// -------------------------------------------------------------------------------------------
// read / list
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn read_returns_the_header_root_fields_counts_and_one_function() {
    let tree = Tree::new("read");
    let file = tree.install("probe.vpcf", PROBE_REL);
    let server = plain_server(&tree);

    let out = call(&server, "particle_read", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["source"], json!("file"));
    assert_eq!(out["path"], json!(PROBE_ARG));
    assert_eq!(out["hostPath"], json!(file.display().to_string()));
    assert_eq!(out["header"], json!(HEADER));
    assert_eq!(out["root"]["_class"], json!("CParticleSystemDefinition"));
    assert_eq!(out["root"]["m_nMaxParticles"], json!(32));
    assert!(out["root"].get("m_Emitters").is_none(), "arrays are not root fields");
    assert_eq!(
        out["counts"],
        json!({"m_Emitters": 1, "m_Initializers": 1, "m_Operators": 2, "m_Renderers": 1})
    );
    assert!(out.get("function").is_none(), "no function was asked for");

    // One entry by reference, and the root (`/`).
    let out = call(
        &server,
        "particle_read",
        json!({"path": PROBE_ARG, "function": "m_Operators/1"}),
    )
    .await
    .unwrap();
    assert_eq!(out["function"]["path"], json!("m_Operators/1"));
    assert_eq!(out["function"]["array"], json!("m_Operators"));
    assert_eq!(out["function"]["type"], json!(3));
    assert_eq!(out["function"]["index"], json!(1));
    assert_eq!(out["function"]["_class"], json!("C_OP_Decay"));
    assert_eq!(out["function"]["fields"], json!({}));

    let out = call(&server, "particle_read", json!({"path": PROBE_ARG, "function": "/"}))
        .await
        .unwrap();
    assert_eq!(out["function"]["path"], json!("/"));
    assert_eq!(out["function"]["array"], Value::Null);
    assert_eq!(out["function"]["fields"]["m_nMaxParticles"], json!(32));

    // The Windows path form of the same file.
    let out = call(&server, "particle_read", json!({"path": PROBE_WIN})).await.unwrap();
    assert_eq!(out["hostPath"], json!(file.display().to_string()));
}

#[tokio::test]
async fn read_negatives_name_the_concrete_failure() {
    let tree = Tree::new("read-neg");
    tree.install("probe.vpcf", PROBE_REL);
    tree.install("not_kv3.vpcf", "particles/mcp_probe/not_kv3.vpcf");
    let server = plain_server(&tree);

    // An unknown array and an entry that does not exist.
    let err = call(
        &server,
        "particle_read",
        json!({"path": PROBE_ARG, "function": "m_Bogus/0"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidArray");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["array"], json!("m_Bogus"));
    assert!(err.retry_safe);
    let err = call(
        &server,
        "particle_read",
        json!({"path": PROBE_ARG, "function": "m_Operators/9"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "FunctionNotFound");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["path"], json!("m_Operators/9"));
    // A reference that is not `<array>/<index>` at all.
    let err = call(&server, "particle_read", json!({"path": PROBE_ARG, "function": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("function"), "{err:?}");

    // A malformed file is a ParseError with the line/col, not an empty read.
    let err = call(
        &server,
        "particle_read",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/not_kv3.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ParseError");
    assert_eq!(err.data["line"], json!(1));
    assert!(err.retry_safe);

    // Wrong extension, unmapped drive, missing file, `..`.
    let err = call(
        &server,
        "particle_read",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert!(err.message.contains("not a .vpcf file"), "{err:?}");
    let err = call(&server, "particle_read", json!({"path": "q:\\nowhere\\probe.vpcf"}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    let err = call(
        &server,
        "particle_read",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/missing.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert!(err.message.contains("cannot be read"), "{err:?}");
    let err = call(
        &server,
        "particle_read",
        json!({"path": "csgo_addons/mcp_test/particles/../mcp_probe/probe.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert!(err.code == Some(-32602));
    // A missing argument is invalid arguments, never a panic.
    let err = call(&server, "particle_read", json!({})).await.unwrap_err();
    assert_eq!(err.code, Some(-32602));
}

#[tokio::test]
async fn list_reports_every_function_and_filters() {
    let tree = Tree::new("list");
    tree.install("probe.vpcf", PROBE_REL);
    tree.install("with_children.vpcf", "particles/mcp_probe/with_children.vpcf");
    let server = plain_server(&tree);

    let out = call(&server, "particle_list", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["source"], json!("file"));
    let functions = out["functions"].as_array().unwrap();
    assert_eq!(out["count"], json!(5), "{functions:?}");
    assert_eq!(functions[0]["array"], json!("m_Emitters"));
    assert_eq!(functions[0]["type"], json!(1));
    assert_eq!(functions[0]["index"], json!(0));
    assert_eq!(functions[0]["class"], json!("C_OP_ContinuousEmitter"));
    assert_eq!(functions[0]["fieldKeys"], json!([]));
    assert_eq!(functions[2]["array"], json!("m_Operators"));
    assert_eq!(functions[3]["class"], json!("C_OP_Decay"));
    assert_eq!(functions[3]["index"], json!(1));
    assert_eq!(functions[4]["class"], json!("C_OP_RenderSprites"));
    assert_eq!(functions[4]["type"], json!(6));
    assert_eq!(out["counts"]["m_Operators"], json!(2));

    // One array, one class, both.
    let out = call(
        &server,
        "particle_list",
        json!({"path": PROBE_ARG, "array": "m_Operators"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(2));
    let out = call(
        &server,
        "particle_list",
        json!({"path": PROBE_ARG, "class": "C_OP_Decay"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["functions"][0]["array"], json!("m_Operators"));
    let out = call(
        &server,
        "particle_list",
        json!({"path": PROBE_ARG, "array": "m_Emitters", "class": "C_OP_Decay"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(0));

    // A structural array (type 7): the entry has no `_class` and one field.
    let out = call(
        &server,
        "particle_list",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/with_children.vpcf", "array": "m_Children"}),
    )
    .await
    .unwrap();
    assert_eq!(out["count"], json!(1));
    assert_eq!(out["functions"][0]["class"], Value::Null);
    assert_eq!(out["functions"][0]["type"], json!(7));
    assert_eq!(out["functions"][0]["fieldKeys"], json!(["m_ChildRef"]));

    // A typo in `array` is refused, not answered with "no entries".
    let err = call(
        &server,
        "particle_list",
        json!({"path": PROBE_ARG, "array": "m_Operator"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("m_Operators"), "{err:?}");
}

// -------------------------------------------------------------------------------------------
// validate
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn validate_reports_ok_against_the_tree_snapshot() {
    let tree = Tree::new("validate-ok");
    tree.install("probe.vpcf", PROBE_REL);
    let snapshot = tree.install_snapshot();
    let server = plain_server(&tree);

    let out = call(&server, "particle_validate", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["errors"], json!([]));
    // The probe fixture has no `m_nBehaviorVersion`: exactly that one warning.
    let warnings = out["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["path"], json!("m_nBehaviorVersion"));
    assert!(warnings[0]["msg"].as_str().unwrap().contains("12"), "{warnings:?}");
    assert_eq!(out["schemaSource"], json!(snapshot.display().to_string()));
    assert_eq!(out["allowObsolete"], json!(true));
}

#[tokio::test]
async fn validate_names_structure_registry_and_field_errors() {
    let tree = Tree::new("validate-errors");
    tree.install_snapshot();
    let server = plain_server(&tree);

    // A wrong root `_class` is structural, reported as ok:false (not a read failure).
    tree.install("bad_root.vpcf", "particles/mcp_probe/bad_root.vpcf");
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/bad_root.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    let errors = out["errors"].as_array().unwrap();
    assert!(
        errors.iter().any(|e| e["path"] == json!("/")
            && e["msg"].as_str().unwrap().contains("CParticleSystemDefinition")),
        "{errors:?}"
    );

    // An unknown class, with the path of the entry.
    tree.write(
        "particles/mcp_neg/unknown_class.vpcf",
        &vpcf_with(&entry("C_OP_Nope", &[])),
    );
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_neg/unknown_class.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["errors"][0]["path"], json!("m_Operators/0"));
    assert!(out["errors"][0]["msg"].as_str().unwrap().contains("C_OP_Nope"));

    // A known class in the wrong array: the message names the arrays it IS registered for.
    tree.write(
        "particles/mcp_neg/wrong_array.vpcf",
        &vpcf_with(&entry("C_OP_RenderSprites", &[])),
    );
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_neg/wrong_array.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["errors"][0]["path"], json!("m_Operators/0"));
    assert!(out["errors"][0]["msg"].as_str().unwrap().contains("m_Renderers"));

    // An unknown field on a known class; a base-class field is fine.
    tree.write(
        "particles/mcp_neg/unknown_field.vpcf",
        &vpcf_with(&entry("C_OP_Decay", &[("m_bogus", "1")])),
    );
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_neg/unknown_field.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert!(out["errors"][0]["msg"].as_str().unwrap().contains("m_bogus"));
    tree.write(
        "particles/mcp_neg/base_field.vpcf",
        &vpcf_with(&entry("C_OP_Decay", &[("m_flOpStartFadeInTime", "0.5")])),
    );
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_neg/base_field.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "a base-class field is a field: {out}");

    // An obsolete class is a warning by default and an error with allowObsolete:false. The
    // probes built here have no `m_nBehaviorVersion`, so the version warning rides along.
    tree.write(
        "particles/mcp_neg/obsolete.vpcf",
        &vpcf_with(&entry("C_OP_TestObsolete", &[])),
    );
    let path = "csgo_addons/mcp_test/particles/mcp_neg/obsolete.vpcf";
    let out = call(&server, "particle_validate", json!({"path": path})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["errors"], json!([]));
    let obsolete = out["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["path"] == json!("m_Operators/0"))
        .expect("the obsolete warning");
    assert!(obsolete["msg"].as_str().unwrap().contains("MObsoleteParticleFunction"), "{obsolete:?}");
    let out = call(
        &server,
        "particle_validate",
        json!({"path": path, "allowObsolete": false}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["errors"][0]["path"], json!("m_Operators/0"));
}

#[tokio::test]
async fn validate_without_a_tree_snapshot_uses_the_built_in_live_snapshot() {
    let tree = Tree::new("validate-noschema");
    tree.install("probe.vpcf", PROBE_REL);
    let server = plain_server(&tree);

    // Nothing in the tree: the committed snapshot (written live from pet, build 2000930) is the
    // fallback, and the result names it. The placeholder refusal is unit-tested in tests/vpcf.rs.
    let out = call(&server, "particle_validate", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["schemaSource"], json!("built-in (build 2000930)"));

    // ... and that fallback is a real schema: an unregistered class is still an error.
    tree.write(
        "particles/mcp_neg/unknown.vpcf",
        &vpcf_with(&entry("C_OP_NoSuchThing", &[])),
    );
    let out = call(
        &server,
        "particle_validate",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_neg/unknown.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["errors"][0]["path"], json!("m_Operators/0"));
}

/// NEGATIVE: a tree snapshot that exists but is corrupt (`not json`) is `SchemaUnavailable`,
/// never a silent pass against the built-in snapshot.
#[tokio::test]
async fn a_corrupt_tree_snapshot_is_schema_unavailable() {
    let tree = Tree::new("validate-corrupt-schema");
    tree.install("probe.vpcf", PROBE_REL);
    tree.write_snapshot("not json");
    let server = plain_server(&tree);

    let err = call(&server, "particle_validate", json!({"path": PROBE_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "SchemaUnavailable", "{err:?}");
    assert_eq!(err.code, Some(-32023));
    assert_eq!(err.data["reason"], json!("schema_invalid"));
    assert!(err.retry_safe);
    // ... and the refusal names the file it came from
    assert!(
        err.message.contains("pet-schema.json"),
        "the refusal must name the snapshot: {err:?}"
    );
}

// -------------------------------------------------------------------------------------------
// edit
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn edit_writes_a_backup_and_the_file_reparses() {
    let tree = Tree::new("edit");
    let file = tree.install("probe.vpcf", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    tree.install_snapshot();
    let server = plain_server(&tree);

    let out = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [
                {"op": "set_root_field", "key": "m_nMaxParticles", "value": 64},
                {"op": "add_function", "array": "m_Operators", "function": {"_class": "C_OP_Decay"}},
            ],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        out["applied"],
        json!([
            {"op": "set_root_field", "path": "/"},
            {"op": "add_function", "path": "m_Operators/2"},
        ])
    );
    assert_eq!(out["appliedCount"], json!(2));
    assert_eq!(out["schemaChecked"], json!(true));
    assert!(out["schemaSource"].as_str().unwrap().ends_with("pet-schema.json"));
    // The only warning comes from the file's missing `m_nBehaviorVersion` (the probe predates
    // the field); the edit itself was clean.
    let warnings = out["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].as_str().unwrap().contains("m_nBehaviorVersion"), "{out}");
    assert_eq!(out["nextStep"], json!("particle_reload if the file is open in pet"));
    let backup = out["backup"].as_str().expect("backup path");
    assert!(backup.contains(".bak-"), "{backup}");
    assert!(Path::new(backup).is_file());
    assert_eq!(
        std::fs::read(backup).unwrap(),
        before,
        "the backup holds the pre-edit file"
    );

    // Re-read from disk: the ops landed, the file validates clean and is KV3 again (a re-parse).
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("m_nMaxParticles = 64"), "{text}");
    assert!(text.contains("C_OP_Decay"), "{text}");
    let read = call(&server, "particle_read", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(read["root"]["m_nMaxParticles"], json!(64));
    assert_eq!(read["counts"]["m_Operators"], json!(3));
    let validated = call(&server, "particle_validate", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(validated["ok"], json!(true), "{validated}");
    assert_eq!(tree.siblings(&file), vec![
        "probe.vpcf".to_string(),
        Path::new(backup).file_name().unwrap().to_string_lossy().into_owned(),
    ]);
}

#[tokio::test]
async fn edit_without_a_tree_snapshot_checks_the_built_in_and_skip_schema_still_writes() {
    let tree = Tree::new("edit-skip");
    let file = tree.install("probe.vpcf", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    let server = plain_server(&tree);
    let ops = json!([{"op": "set_root_field", "key": "m_nMaxParticles", "value": 64}]);

    // No tree snapshot: the built-in live snapshot still checks the edit; an unknown class is
    // refused and nothing is written.
    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [{"op": "add_function", "array": "m_Operators", "function": {"_class": "C_OP_NoSuchThing"}}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ValidationFailed");
    assert_eq!(std::fs::read(&file).unwrap(), before, "file byte-identical");
    assert_eq!(tree.siblings(&file), vec!["probe.vpcf".to_string()], "no backup");

    // skipSchema skips only the schema check, and the result says so.
    let out = call(
        &server,
        "particle_edit",
        json!({"path": PROBE_ARG, "ops": ops, "skipSchema": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["schemaChecked"], json!(false));
    assert_eq!(out["schemaSource"], Value::Null);
    assert!(out["backup"].as_str().unwrap().contains(".bak-"));
    assert!(out["warnings"][0].as_str().unwrap().contains("skipSchema"));
    let read = call(&server, "particle_read", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(read["root"]["m_nMaxParticles"], json!(64));

    // Structural errors are NOT skippable: a file whose root class is wrong is refused either way.
    let text = vpcf_with("").replace("CParticleSystemDefinition", "CParticleSystem");
    let bad = tree.write("particles/mcp_neg/bad_root.vpcf", &text);
    let before = std::fs::read(&bad).unwrap();
    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": "csgo_addons/mcp_test/particles/mcp_neg/bad_root.vpcf",
            "ops": [{"op": "set_root_field", "key": "m_nMaxParticles", "value": 4}],
            "skipSchema": true,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ValidationFailed");
    assert_eq!(err.data["issues"][0]["path"], json!("/"));
    assert_eq!(std::fs::read(&bad).unwrap(), before, "file byte-identical");
}

#[tokio::test]
async fn edit_with_an_unknown_class_is_refused_and_changes_nothing() {
    let tree = Tree::new("edit-unknown");
    let file = tree.install("probe.vpcf", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    tree.install_snapshot();
    let server = plain_server(&tree);

    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [{"op": "add_function", "array": "m_Operators", "function": {"_class": "C_OP_Nope"}}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ValidationFailed");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["issues"][0]["path"], json!("m_Operators/2"));
    assert!(err.data["issues"][0]["msg"].as_str().unwrap().contains("C_OP_Nope"));
    assert!(err.retry_safe, "nothing was written");
    assert_eq!(std::fs::read(&file).unwrap(), before, "file byte-identical");
    assert_eq!(
        tree.siblings(&file),
        vec!["probe.vpcf".to_string()],
        "no backup and no temporary was created"
    );

    // allowObsolete:false turns the obsolete warning into the same refusal.
    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "allowObsolete": false,
            "ops": [{"op": "add_function", "array": "m_Operators", "function": {"_class": "C_OP_TestObsolete"}}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "ValidationFailed");
    assert!(err.data["issues"][0]["msg"].as_str().unwrap().contains("obsolete"));
    assert_eq!(std::fs::read(&file).unwrap(), before);

    // The same op with allowObsolete defaulting to true writes; among the warnings is the
    // obsolete one (alongside the missing `m_nBehaviorVersion` warning).
    let out = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [{"op": "add_function", "array": "m_Operators", "function": {"_class": "C_OP_TestObsolete"}}],
        }),
    )
    .await
    .unwrap();
    assert!(out["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("obsolete")), "{out}");
}

#[tokio::test]
async fn edit_op_failure_rolls_back_and_writes_nothing() {
    let tree = Tree::new("edit-atomic");
    let file = tree.install("probe.vpcf", PROBE_REL);
    let before = std::fs::read(&file).unwrap();
    tree.install_snapshot();
    let server = plain_server(&tree);

    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": PROBE_ARG,
            "ops": [
                {"op": "set_root_field", "key": "m_nMaxParticles", "value": 99},
                {"op": "remove_function", "array": "m_Operators", "index": 9},
            ],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidOp");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(err.data["opIndex"], json!(1));
    assert!(err.retry_safe, "nothing was written");
    assert_eq!(std::fs::read(&file).unwrap(), before, "file byte-identical");
    assert_eq!(tree.siblings(&file), vec!["probe.vpcf".to_string()]);

    // Argument negatives: an unknown op, an empty ops list, too many ops, a bad skipSchema.
    for (args, wrong) in [
        (json!({"path": PROBE_ARG, "ops": []}), "empty ops"),
        (json!({"path": PROBE_ARG, "ops": [{"op": "bogus"}]}), "unknown op"),
        (
            json!({"path": PROBE_ARG, "ops": [{"op": "set_root_field", "key": "m_Emitters", "value": 1}]}),
            "structural root key",
        ),
        (
            json!({"path": PROBE_ARG, "skipSchema": "yes", "ops": [{"op": "set_root_field", "key": "m_nMaxParticles", "value": 1}]}),
            "bad skipSchema",
        ),
        (json!({"path": PROBE_ARG}), "missing ops"),
    ] {
        let err = call(&server, "particle_edit", args).await.unwrap_err();
        assert_eq!(err.code, Some(-32602), "case {wrong}: {err:?}");
    }
}

#[tokio::test]
async fn edit_outside_the_addon_is_refused() {
    let tree = Tree::new("edit-outside");
    tree.install_snapshot();
    let server = plain_server(&tree);
    // Core content is under content/, but not under content/csgo_addons/mcp_test/.
    let core = tree.app.join("content/csgo/particles/core_probe.vpcf");
    std::fs::create_dir_all(core.parent().unwrap()).unwrap();
    std::fs::write(&core, "irrelevant").unwrap();

    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": "csgo/particles/core_probe.vpcf",
            "ops": [{"op": "set_root_field", "key": "m_nMaxParticles", "value": 1}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "WriteOutsideAddon");
    assert_eq!(err.code, Some(-32602));
    assert_eq!(std::fs::read_to_string(&core).unwrap(), "irrelevant", "nothing written");

    let err = call(
        &server,
        "particle_edit",
        json!({
            "path": "q:\\nowhere\\probe.vpcf",
            "ops": [{"op": "set_root_field", "key": "m_nMaxParticles", "value": 1}],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "PathNotMapped");
    assert!(err.retry_safe);
}

// -------------------------------------------------------------------------------------------
// create
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_writes_the_template_and_refuses_an_existing_file() {
    let tree = Tree::new("create");
    tree.install_snapshot();
    let server = plain_server(&tree);
    let rel = "particles/mcp_new/probe.vpcf";
    let arg = "csgo_addons/mcp_test/particles/mcp_new/probe.vpcf";

    let out = call(&server, "particle_create", json!({"path": arg})).await.unwrap();
    assert_eq!(out["created"], json!(true));
    assert_eq!(out["backup"], Value::Null, "nothing to back up");
    assert_eq!(out["classes"], json!(TEMPLATE_CLASSES));
    assert_eq!(out["classesVerified"], json!(true));
    assert_eq!(out["warnings"], json!([]));
    assert!(out.get("nextStep").is_none(), "{out}");
    let next = out["next"].as_array().expect("next array");
    assert_eq!(next[1]["tool"], json!("particle_compile"), "{out}");
    assert_eq!(next[1]["args"]["path"], json!(arg), "{out}");
    // The template with defaults is the probe fixture plus exactly the m_nBehaviorVersion line,
    // byte for byte (expectation constructed from the fixture text, not a second copy).
    let file = tree.addon().join(rel);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        probe_created_text().into_bytes(),
    );

    let read = call(&server, "particle_read", json!({"path": arg})).await.unwrap();
    assert_eq!(read["counts"]["m_Operators"], json!(2));

    // Second create refuses; overwrite is a backup + rewrite.
    let err = call(&server, "particle_create", json!({"path": arg})).await.unwrap_err();
    assert_eq!(err.name, "FileExists");
    assert_eq!(err.code, Some(-32602));
    assert!(err.retry_safe);
    let out = call(
        &server,
        "particle_create",
        json!({"path": arg, "overwrite": true, "params": {"maxParticles": 8}}),
    )
    .await
    .unwrap();
    assert_eq!(out["created"], json!(true));
    assert!(out["backup"].as_str().expect("overwrite backup").contains(".bak-"));
    let read = call(&server, "particle_read", json!({"path": arg})).await.unwrap();
    assert_eq!(read["root"]["m_nMaxParticles"], json!(8));
}

#[tokio::test]
async fn create_verifies_classes_against_the_built_in_and_refuses_a_missing_template_class() {
    // No tree snapshot: the built-in live snapshot verifies the template's classes.
    let tree = Tree::new("create-noschema");
    let server = plain_server(&tree);
    let out = call(
        &server,
        "particle_create",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_new/probe.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["created"], json!(true));
    assert_eq!(out["classesVerified"], json!(true), "{out}");
    assert!(tree.addon().join("particles/mcp_new/probe.vpcf").is_file());

    // A usable snapshot that lacks a class the template emits refuses the create.
    let tree = Tree::new("create-missing-class");
    let mut snapshot: Value = serde_json::from_str(SCHEMA_TEST).unwrap();
    snapshot["classes"].as_object_mut().unwrap().remove("C_OP_RenderSprites");
    tree.write_snapshot(&snapshot.to_string());
    let server = plain_server(&tree);
    let err = call(
        &server,
        "particle_create",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_new/probe.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "UnknownClass");
    assert_eq!(err.data["class"], json!("C_OP_RenderSprites"));
    assert!(!tree.addon().join("particles/mcp_new/probe.vpcf").exists(), "nothing written");

    // A template argument that is not `minimal` is invalid arguments.
    let tree = Tree::new("create-badtemplate");
    let server = plain_server(&tree);
    let err = call(
        &server,
        "particle_create",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_new/probe.vpcf", "template": "fancy"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("minimal"), "{err:?}");
}

// -------------------------------------------------------------------------------------------
// compile (fake podman)
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_resourcecompiler_ok() {
    let tree = Tree::new("rc-ok");
    tree.install("probe.vpcf", PROBE_REL);
    let podman = fake_podman(&tree, "ok");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    let out = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["route"], json!("resourcecompiler"));
    assert_eq!(out["ok"], json!(true));
    assert_eq!(out["compiled"], json!(1));
    assert_eq!(out["failed"], json!(0));
    assert_eq!(out["skipped"], json!(0));
    assert_eq!(out["fatal"], json!(false));
    assert_eq!(out["outputUpdated"], json!(true));
    assert_eq!(out["container"], json!("cs2tools"));
    assert_eq!(
        out["outputPath"],
        json!(tree.compile_output(PROBE_REL).display().to_string())
    );
    assert!(out["stdoutTail"].as_str().unwrap().contains("OK: 1 compiled"));
    assert!(tree.compile_output(PROBE_REL).is_file(), "the fake compiler touched it");
}

#[tokio::test]
async fn compile_failure_and_skip_are_results() {
    let tree = Tree::new("rc-fail");
    tree.install("probe.vpcf", PROBE_REL);
    let podman = fake_podman(&tree, "fail");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    let out = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["fatal"], json!(true));
    assert_eq!(out["compiled"], Value::Null, "no OK summary");
    assert_eq!(
        out["failLines"],
        json!(["[FAIL] dependency failed: particles/missing/missing.vpcf"])
    );
    assert_eq!(out["outputUpdated"], json!(false));

    // A skip is only a success when the output is at least as new as the source.
    let tree = Tree::new("rc-skip");
    tree.install("probe.vpcf", PROBE_REL);
    let podman = fake_podman(&tree, "skipped");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(false));
    assert_eq!(out["upToDate"], json!(false));
    // the single-file files[0] carries the same skippedByCompiler/reason as the top level
    assert_eq!(out["files"][0]["skippedByCompiler"], out["skippedByCompiler"], "{out}");
    assert!(out["files"][0]["reason"].as_str().is_some_and(|r| !r.is_empty()), "{out}");
    let output = tree.compile_output(PROBE_REL);
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"compiled").unwrap();
    let out = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["upToDate"], json!(true));
    assert_eq!(out["outputUpdated"], json!(false));
}

#[tokio::test]
async fn compile_without_a_tools_container_is_runtime_unavailable() {
    let tree = Tree::new("rc-none");
    tree.install("probe.vpcf", PROBE_REL);

    // Hermetic default: podman does not exist at all.
    let server = plain_server(&tree);
    let err = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert_eq!(err.data["label"], json!("cs2mcp.tool"));
    assert_eq!(err.data["hint"], json!("bridge/runtime.sh up"));
    assert!(err.message.contains("cs2mcp.tool"), "{err:?}");
    assert!(err.retry_safe);

    // A running podman that finds no container with the label.
    let podman = fake_podman(&tree, "no-container");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let err = call(&server, "particle_compile", json!({"path": PROBE_ARG})).await.unwrap_err();
    assert_eq!(err.name, "RuntimeUnavailable");
    assert!(err.message.contains("cs2mcp.tool"), "{err:?}");
}

#[tokio::test]
async fn compile_negatives_are_invalid_params() {
    let tree = Tree::new("rc-neg");
    tree.install("probe.vpcf", PROBE_REL);
    let podman = fake_podman(&tree, "ok");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    // A `.vmdl` is not a `.vpcf`.
    let err = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vmdl"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.name, "InvalidPath");
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("not a .vpcf file"), "{err:?}");

    // A missing file.
    let err = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/missing.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("does not exist"), "{err:?}");

    // A container path that could become shell code is refused before podman runs.
    tree.install("probe.vpcf", "particles/w$ird/probe.vpcf");
    let err = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/w$ird/probe.vpcf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("shell"), "{err:?}");
    assert!(
        !tree.compile_output("particles/w$ird/probe.vpcf").exists(),
        "no compile ran"
    );

    // timeoutMs validation, and budget exhaustion (a zero remainder).
    let err = call(
        &server,
        "particle_compile",
        json!({"path": PROBE_ARG, "timeoutMs": 0}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    assert!(err.message.contains("timeoutMs"), "{err:?}");
    let err = call(
        &server,
        "particle_compile",
        json!({"path": PROBE_ARG, "timeoutMs": "soon"}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602));
    // A budget that is already gone is budget exhaustion, not a "forever" compile.
    let starved = compile_server(&tree, Some(podman.display().to_string()), 0);
    let err = call(&starved, "particle_compile", json!({"path": PROBE_ARG}))
        .await
        .unwrap_err();
    assert_eq!(err.name, "ToolBudgetExceeded");
    assert!(err.retry_safe);
}


// -------------------------------------------------------------------------------------------
// particle_compile: folders and children
// -------------------------------------------------------------------------------------------

/// A minimal `.vpcf` whose `m_Children` reference `refs` (addon-relative paths).
fn vpcf_with_children(refs: &[&str]) -> String {
    let children: String = refs
        .iter()
        .map(|r| format!("\t\t{{\n\t\t\tm_ChildRef = resource:\"{r}\"\n\t\t}},\n"))
        .collect();
    format!(
        "{HEADER}\n{{\n\t_class = \"CParticleSystemDefinition\"\n\tm_nMaxParticles = 8\n\tm_Children = \n\t[\n{children}\t]\n}}"
    )
}

/// The exec log of the `multi` fake: one command line per exec.
fn exec_log(tree: &Tree) -> Vec<String> {
    std::fs::read_to_string(tree.root.join("exec.log"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn file_verdict<'a>(out: &'a Value, path: &str) -> &'a Value {
    out["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == json!(path))
        .unwrap_or_else(|| panic!("no verdict for {path} in {out}"))
}

#[tokio::test]
async fn compile_pulls_in_children_first_in_one_run() {
    let tree = Tree::new("rc-children");
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/child.vpcf"]));
    tree.write("particles/t/child.vpcf", &vpcf_with_children(&["particles/t/leaf.vpcf"]));
    tree.write("particles/t/leaf.vpcf", &vpcf_with_children(&[]));
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    let order: Vec<&str> =
        out["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(
        order,
        ["particles/t/leaf.vpcf", "particles/t/child.vpcf", "particles/t/parent.vpcf"],
        "children first"
    );
    assert_eq!(
        file_verdict(&out, "particles/t/child.vpcf")["referencedBy"],
        json!("particles/t/parent.vpcf")
    );
    let log = exec_log(&tree);
    assert_eq!(log.len(), 1, "one compiler run: {log:?}");
    for name in ["leaf", "child", "parent"] {
        assert!(log[0].contains(&format!("-i /app/content/csgo_addons/mcp_test/particles/t/{name}.vpcf")), "{}", log[0]);
        assert!(tree.compile_output(&format!("particles/t/{name}.vpcf")).is_file());
    }

    // children:false compiles the parent alone (the single-file result shape)
    std::fs::remove_file(tree.root.join("exec.log")).unwrap();
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf", "children": false}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["files"].as_array().unwrap().len(), 1);
    assert!(!exec_log(&tree)[0].contains("child.vpcf"));
}

#[tokio::test]
async fn compile_missing_child_is_a_per_file_failure_naming_the_parent() {
    let tree = Tree::new("rc-missing-child");
    // readable VPKs: the lookup ran and found nothing, which is what "missing" means
    vpk_pair(&tree, &[common::BuildFile::preload_only("particles/other.vpcf_c", b"x")]);
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/gone.vpcf"]));
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    let gone = file_verdict(&out, "particles/t/gone.vpcf");
    assert_eq!(gone["ok"], json!(false));
    assert_eq!(gone["referencedBy"], json!("particles/t/parent.vpcf"));
    assert!(
        gone["failLines"][0].as_str().unwrap().contains("referenced by particles/t/parent.vpcf"),
        "{gone}"
    );
    assert_eq!(file_verdict(&out, "particles/t/parent.vpcf")["ok"], json!(true));
    let log = exec_log(&tree);
    assert_eq!(log.len(), 1);
    assert!(!log[0].contains("gone.vpcf"), "the missing file is never sent: {}", log[0]);
}

#[tokio::test]
async fn compile_reference_cycle_terminates_and_each_file_compiles_once() {
    let tree = Tree::new("rc-cycle");
    tree.write("particles/t/a.vpcf", &vpcf_with_children(&["particles/t/b.vpcf", "particles/t/a.vpcf"]));
    tree.write("particles/t/b.vpcf", &vpcf_with_children(&["particles/t/a.vpcf"]));
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/a.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["files"].as_array().unwrap().len(), 2, "{out}");
    let log = exec_log(&tree);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].matches("-i ").count(), 2, "{}", log[0]);
}

#[tokio::test]
async fn compile_folder_lists_vpcf_and_recursive_descends() {
    let tree = Tree::new("rc-folder");
    tree.write("particles/t/a.vpcf", &vpcf_with_children(&[]));
    tree.write("particles/t/b.vpcf", &vpcf_with_children(&["particles/t/sub/c.vpcf"]));
    tree.write("particles/t/sub/c.vpcf", &vpcf_with_children(&[]));
    tree.write("particles/t/sub/d.vpcf", &vpcf_with_children(&[]));
    tree.write("particles/t/notes.txt", "x");
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    // not recursive: a and b, plus b's child c (children default on); d and notes.txt are not in
    let out = call(&server, "particle_compile", json!({"path": "csgo_addons/mcp_test/particles/t"}))
        .await
        .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    let mut names: Vec<&str> =
        out["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["particles/t/a.vpcf", "particles/t/b.vpcf", "particles/t/sub/c.vpcf"]);

    // recursive: d joins
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/", "recursive": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["files"].as_array().unwrap().len(), 4, "{out}");
    assert_eq!(exec_log(&tree).len(), 2, "one run per call");

    // an empty folder and recursive on a file are refused
    std::fs::create_dir_all(tree.addon().join("particles/empty")).unwrap();
    let err = call(&server, "particle_compile", json!({"path": "csgo_addons/mcp_test/particles/empty"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    let err = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/a.vpcf", "recursive": true}),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, Some(-32602), "{err:?}");
    assert_eq!(exec_log(&tree).len(), 2, "the refusals ran nothing");
}

#[tokio::test]
async fn compile_outside_the_addon_is_refused_and_nothing_runs() {
    let tree = Tree::new("rc-outside");
    tree.write("particles/t/a.vpcf", &vpcf_with_children(&[]));
    std::fs::create_dir_all(tree.app.join("content/csgo_addons/other/particles")).unwrap();
    std::fs::create_dir_all(tree.app.join("content/core/particles")).unwrap();
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);

    for path in [
        "csgo_addons/other/particles",
        "core/particles",
        "csgo_addons/mcp_test/particles/../../other/particles",
    ] {
        let err = call(&server, "particle_compile", json!({"path": path})).await.unwrap_err();
        assert!(
            err.name == "WriteOutsideAddon" || err.name == "InvalidPath",
            "{path}: {err:?}"
        );
        assert_eq!(err.code, Some(-32602), "{path}");
    }
    // a child reference that climbs out is a per-file failure, never compiled
    tree.write("particles/t/esc.vpcf", &vpcf_with_children(&["../../other/particles/x.vpcf"]));
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/esc.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    let log = exec_log(&tree);
    assert!(log.iter().all(|l| !l.contains("other")), "{log:?}");
    assert_eq!(log.len(), 1, "only the in-addon parent ran");
}

fn vpk_pair(tree: &Tree, csgo_files: &[common::BuildFile]) {
    common::write_vpk(&tree.app.join("game/csgo"), "pak01", 2, csgo_files);
    common::write_vpk(
        &tree.app.join("game/core"),
        "pak01",
        1,
        &[common::BuildFile::preload_only("particles/core_only.vpcf_c", b"x")],
    );
}

#[tokio::test]
async fn compile_external_children_are_reported_and_never_compiled() {
    let tree = Tree::new("rc-external");
    vpk_pair(
        &tree,
        &[common::BuildFile::preload_only("particles/Explosions/Boom.vpcf_c", b"x")],
    );
    // a stock source in content/csgo, a compiled-only addon file, a case-different addon source
    let stock = tree.app.join("content/csgo/particles/stock/glow.vpcf");
    std::fs::create_dir_all(stock.parent().unwrap()).unwrap();
    std::fs::write(&stock, vpcf_with_children(&[])).unwrap();
    let compiled_only = tree.compile_output("particles/t/prebuilt.vpcf");
    std::fs::create_dir_all(compiled_only.parent().unwrap()).unwrap();
    std::fs::write(&compiled_only, b"c").unwrap();
    tree.write("particles/t/Local.vpcf", &vpcf_with_children(&[]));
    tree.write(
        "particles/t/parent.vpcf",
        &vpcf_with_children(&[
            "particles/stock/glow.vpcf",
            "particles/explosions/boom.vpcf",
            "particles/core_only.vpcf",
            "particles/t/prebuilt.vpcf",
            "particles/t/local.vpcf",
        ]),
    );
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    let v = file_verdict(&out, "particles/stock/glow.vpcf");
    assert_eq!((v["external"].clone(), v["source"].clone()), (json!(true), json!("content/csgo")));
    assert_eq!(file_verdict(&out, "particles/explosions/boom.vpcf")["source"], json!("vpk:csgo"));
    assert_eq!(file_verdict(&out, "particles/core_only.vpcf")["source"], json!("vpk:core"));
    let v = file_verdict(&out, "particles/t/prebuilt.vpcf");
    assert_eq!((v["source"].clone(), v["compiled"].clone()), (json!("addon-compiled"), json!(false)));
    let log = exec_log(&tree);
    assert_eq!(log.len(), 1);
    for external in ["glow", "boom", "core_only", "prebuilt"] {
        assert!(!log[0].contains(external), "{external} must not be compiled: {}", log[0]);
    }
    // the case-different addon source IS compiled (as the file's real name)
    assert!(log[0].contains("particles/t/Local.vpcf"), "{}", log[0]);
    assert_eq!(out["files"].as_array().unwrap().len(), 6, "{out}");
}

#[tokio::test]
async fn compile_child_found_nowhere_still_fails() {
    let tree = Tree::new("rc-nowhere");
    vpk_pair(&tree, &[common::BuildFile::preload_only("particles/other.vpcf_c", b"x")]);
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/nope.vpcf"]));
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(file_verdict(&out, "particles/nope.vpcf")["referencedBy"], json!("particles/t/parent.vpcf"));
}

#[tokio::test]
async fn compile_a_fail_line_names_one_file_and_the_count_must_add_up() {
    let tree = Tree::new("rc-fail-one");
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/bad.vpcf", "particles/t/fine.vpcf"]));
    tree.write("particles/t/bad.vpcf", &vpcf_with_children(&[]));
    tree.write("particles/t/fine.vpcf", &vpcf_with_children(&[]));
    let podman = fake_podman(&tree, "multi-fail");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(file_verdict(&out, "particles/t/bad.vpcf")["ok"], json!(false));
    assert_eq!(file_verdict(&out, "particles/t/fine.vpcf")["ok"], json!(true));
    assert_eq!(file_verdict(&out, "particles/t/parent.vpcf")["ok"], json!(true));

    // the compiler reports fewer files than were sent: not accounted for, even if outputs exist
    let tree = Tree::new("rc-short");
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/c.vpcf"]));
    tree.write("particles/t/c.vpcf", &vpcf_with_children(&[]));
    let podman = fake_podman(&tree, "multi-short");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "{out}");
    assert!(
        out["reason"].as_str().unwrap().contains("did not account for every input"),
        "{out}"
    );
    // a stale-but-newer output is not proof either: pre-existing outputs, nothing refreshed
    let tree = Tree::new("rc-stale");
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/c.vpcf"]));
    tree.write("particles/t/c.vpcf", &vpcf_with_children(&[]));
    for name in ["parent", "c"] {
        let o = tree.compile_output(&format!("particles/t/{name}.vpcf"));
        std::fs::create_dir_all(o.parent().unwrap()).unwrap();
        std::fs::write(&o, b"old").unwrap();
    }
    let podman = fake_podman(&tree, "skipped");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(false), "1 skipped reported for 2 inputs: {out}");
}

#[cfg(unix)]
#[tokio::test]
async fn compile_folder_walk_notes_skipped_symlinks() {
    let tree = Tree::new("rc-symlink");
    tree.write("particles/t/a.vpcf", &vpcf_with_children(&[]));
    tree.write("particles/elsewhere/z.vpcf", &vpcf_with_children(&[]));
    std::os::unix::fs::symlink(
        tree.addon().join("particles/elsewhere"),
        tree.addon().join("particles/t/link"),
    )
    .unwrap();
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t", "recursive": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["files"].as_array().unwrap().len(), 1, "{out}");
    assert!(out["notes"][0].as_str().unwrap().contains("link"), "{out}");
}

#[tokio::test]
async fn compile_count_is_at_least_the_inputs_because_dependencies_count_too() {
    let tree = Tree::new("rc-more");
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/t/c.vpcf"]));
    tree.write("particles/t/c.vpcf", &vpcf_with_children(&[]));
    let podman = fake_podman(&tree, "multi-more");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["compiled"], json!(3));
    assert_eq!(out["ok"], json!(true), "2 inputs, 3 compiled (a shared dependency): {out}");
}

#[tokio::test]
async fn compile_with_unreadable_game_vpks_passes_stock_children_unverified() {
    let tree = Tree::new("rc-novpk");
    // no game/csgo|core pak01_dir.vpk anywhere in the tree
    tree.write("particles/t/parent.vpcf", &vpcf_with_children(&["particles/stock/glow.vpcf"]));
    let podman = fake_podman(&tree, "multi");
    let server = compile_server(&tree, Some(podman.display().to_string()), 45_000);
    let out = call(
        &server,
        "particle_compile",
        json!({"path": "csgo_addons/mcp_test/particles/t/parent.vpcf"}),
    )
    .await
    .unwrap();
    assert_eq!(out["ok"], json!(true), "{out}");
    let v = file_verdict(&out, "particles/stock/glow.vpcf");
    assert_eq!(v["external"], json!(true));
    assert_eq!(v["source"], json!("unknown"));
    assert_eq!(v["verified"], json!(false));
    assert!(out["notes"][0].as_str().unwrap().contains("could not be read"), "{out}");
    assert!(!exec_log(&tree)[0].contains("glow"));
}
