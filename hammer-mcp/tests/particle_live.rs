//! Mock-backed tests for the live particle tools
//! (`particle_status/types/schema/load/open/reload/save`) against the mock bridge's `pet.*`
//! built-ins (m2.12 semantics, WP2) and the `vpcf` layer (WP3).
//!
//! The temp project tree mimics the real layout: `--project-root` is the temp root, so the content
//! lives at `<root>/workshop/run/app/content/…` (PathMap's default `z:\app` mapping) and the
//! schema snapshot at `<root>/hammer-mcp/research/pet-schema.json`.
//!
//! The tools are registered directly here (`particle_live::register`) because the orchestrator wires
//! `tools/particles.rs` (PET_TOOL_ORDER) only at merge time; the registry under test is the real
//! one, so capability gating is exercised exactly as in production.
#![allow(clippy::result_large_err)]

use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::mock::{make_pet_doc, MockBridge};
use cs2_mcp_common::tools::{Budget, CallFailure, Registry, ToolError};
use hammer_mcp::state::HammerState;
use hammer_mcp::tools::particle_live::{self, PARTICLE_LIVE_TOOL_ORDER};
use hammer_mcp::vpcf::Schema;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- fixtures

/// The live-verified probe `.vpcf` (Valve KV3 layout, CRLF): the file route's canonical fixture.
const PROBE: &str = include_str!("../fixtures/vpcf/probe.vpcf");
/// WP3's hand-made 0.3 snapshot (`placeholder:false`, build "test", the probe classes plus an
/// obsolete operator and a class registered under two types).
const SNAPSHOT: &str = include_str!("../fixtures/vpcf/schema-test.json");
/// The probe window path the mock's `pet.open`/`pet.active` report.
const DOC_WIN: &str = "Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf";

/// A scratch project tree under `target/tmp` (never `/tmp`), unique per test.
struct Host {
    dir: PathBuf,
}
impl Host {
    fn new(tag: &str) -> Host {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("particle-live-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Host { dir }
    }
    fn addon(&self) -> PathBuf {
        self.dir.join("workshop/run/app/content/csgo_addons/mcp_test")
    }
    /// Write a `.vpcf` inside the active addon (`particles/…`).
    fn write_particle(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.addon().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    /// A `.vpcf` inside the content root but outside the active addon (core content).
    fn write_core_particle(&self, rel: &str, text: &str) -> PathBuf {
        let path = self
            .dir
            .join("workshop/run/app/content/csgo")
            .join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
    /// The host path of the probe the mock's `pet.open` reports (addon-relative `particles/…`).
    fn probe_host(&self) -> PathBuf {
        self.addon().join("particles/mcp_probe/probe.vpcf")
    }
    /// The snapshot the tools write/read under `--project-root`'s checkout.
    fn snapshot(&self) -> PathBuf {
        self.dir.join("hammer-mcp/research/pet-schema.json")
    }
    /// Install WP3's 0.3 snapshot where `particle_open`/`particle_reload` look for it.
    fn write_snapshot(&self) -> PathBuf {
        let path = self.snapshot();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, SNAPSHOT).unwrap();
        path
    }
    /// The probe plus a valid schema, the baseline a successful open/reload needs.
    fn write_probe(&self) -> PathBuf {
        self.write_snapshot();
        self.write_particle("particles/mcp_probe/probe.vpcf", PROBE)
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn config(discovery: &Path, host_dir: &Path) -> CommonConfig {
    use clap::Parser;
    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        c: CommonConfig,
    }
    let mut argv = vec![
        "t".to_string(),
        "--discovery".to_string(),
        discovery.display().to_string(),
        "--project-root".to_string(),
        host_dir.display().to_string(),
    ];
    // hermetic podman/vk-layer (the pattern of tests/common/mod.rs, copied, not imported):
    // nothing here may touch the host's real podman or project tree.
    argv.extend([
        "--podman".into(),
        "/nonexistent/cs2mcp-test-podman".into(),
        "--vk-layer-dir".into(),
        "/nonexistent/cs2mcp-test-vklayer".into(),
    ]);
    W::parse_from(argv).c
}

/// A real registry (capability gating included) with the seven live particle tools registered.
struct Server {
    state: HammerState,
    registry: Registry<HammerState>,
}

impl Server {
    fn new(discovery: &Path, host_dir: &Path) -> Server {
        let state = HammerState::from_config(&config(discovery, host_dir)).unwrap();
        let mut registry = Registry::new();
        particle_live::register(&mut registry);
        Server { state, registry }
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let budget = Budget::new(45_000, 30_000);
        match self.registry.call(&self.state, name, args, budget).await {
            Ok(out) => Ok(out.json),
            Err(CallFailure::Tool(e)) => Err(e),
            Err(CallFailure::UnknownTool(n)) => panic!("unknown tool {n}"),
        }
    }
}

/// The `deadlineMs` of every recorded `pet.*` request (asserted > 0 across all tests).
fn pet_deadlines(mb: &MockBridge) -> Vec<u64> {
    mb.requests()
        .into_iter()
        .filter(|r| r.method.starts_with("pet."))
        .map(|r| r.params["deadlineMs"].as_u64().unwrap_or(0))
        .collect()
}

// ---------------------------------------------------------------- registration sanity

#[test]
fn the_seven_live_particle_tools_are_registered() {
    let mut registry = Registry::<HammerState>::new();
    particle_live::register(&mut registry);
    assert_eq!(registry.names(), PARTICLE_LIVE_TOOL_ORDER);
    // particle_save declares its one optional knob in the input schema
    let save = registry.spec("particle_save").unwrap();
    assert_eq!(
        save.input_schema["properties"]["force"]["type"],
        json!("boolean")
    );
    assert_eq!(save.input_schema["required"], Value::Null);
}

// ---------------------------------------------------------------- particle_status

#[tokio::test]
async fn status_without_the_module_reports_pet_not_loaded_and_a_hint() {
    let host = Host::new("status-notloaded");
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(true));
    assert_eq!(p["petLoaded"], json!(false));
    assert_eq!(p["capabilities"]["missing"].as_array().unwrap().len(), 6);
    let hint = p["hint"].as_str().unwrap();
    assert!(hint.ends_with("runtime.sh up; then particle_load"), "{hint}");
    // nothing pet.* was probed (pet.active is not advertised yet)
    assert!(mb.method_calls("pet.active").is_empty());
}

#[tokio::test]
async fn status_not_connected_never_raises() {
    let host = Host::new("status-dead");
    let server = Server::new(Path::new("/nonexistent/cs2mcp-no-bridge/session.json"), &host.dir);
    let p = server.call("particle_status", json!({})).await.unwrap();
    assert_eq!(p["bridge"]["running"], json!(false));
    assert_eq!(p["petLoaded"], json!(false));
    assert!(p["hint"].as_str().unwrap().ends_with("runtime.sh up"));
    assert!(p.get("document").is_none());
    // the seven methods are reported missing even without a connection
    assert_eq!(p["capabilities"]["missing"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn status_loaded_shows_the_document_with_mapped_arrays() {
    let host = Host::new("status-loaded");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_status", json!({})).await.unwrap();
    assert_eq!(p["petLoaded"], json!(true));
    assert_eq!(p["capabilities"]["missing"], json!([]));
    assert_eq!(p["document"]["path"], json!(DOC_WIN));
    assert_eq!(
        p["document"]["hostPath"],
        json!(host.probe_host().display().to_string())
    );
    assert_eq!(p["document"]["pathMapped"], json!(true));
    assert_eq!(p["document"]["inAddon"], json!(true));
    assert_eq!(p["document"]["hasRoot"], json!(true));
    // the bridge's integer type index is mapped to the file key, classes preserved
    assert_eq!(p["document"]["arrays"]["m_Emitters"], json!(["C_OP_ContinuousEmitter"]));
    assert_eq!(p["document"]["arrays"]["m_Operators"], json!(["C_OP_BasicMovement", "C_OP_Decay"]));
    assert_eq!(p["document"]["arrays"]["m_Renderers"], json!(["C_OP_RenderSprites"]));
    assert_eq!(p["document"]["arrays"]["m_Children"], json!([]));
    assert_eq!(p["hint"], Value::Null);
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn status_with_no_document_reports_the_bridge_reason() {
    let host = Host::new("status-nodoc");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_status", json!({})).await.unwrap();
    assert_eq!(p["petLoaded"], json!(true));
    assert_eq!(p["document"], Value::Null);
    assert_eq!(p["documentReason"], json!("no_session"));
}

// ---------------------------------------------------------------- particle_load

#[tokio::test]
async fn load_reports_not_already_loaded_then_advertises_the_six() {
    let host = Host::new("load-fresh");
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_load", json!({})).await.unwrap();
    assert_eq!(
        p,
        json!({"loaded": true, "alreadyLoaded": false, "toolIndex": 3, "module": "verified"})
    );
    // the handler refreshed control.capabilities after the load (the bridge bumps the revision)
    assert!(!mb.method_calls("control.capabilities").is_empty());
    // the six NEED_PET methods are advertised now; status sees the module as loaded
    let status = server.call("particle_status", json!({})).await.unwrap();
    assert_eq!(status["petLoaded"], json!(true));
    assert_eq!(status["capabilities"]["missing"], json!([]));
    // a second load reports alreadyLoaded with no tool index
    let second = server.call("particle_load", json!({})).await.unwrap();
    assert_eq!(second["alreadyLoaded"], json!(true));
    assert_eq!(second["toolIndex"], Value::Null);
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn load_without_the_capability_is_not_implemented_and_sends_nothing() {
    let host = Host::new("load-gated");
    let mb = MockBridge::builder().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_load", json!({})).await.unwrap_err();
    assert_eq!(e.name, "NotImplemented");
    assert_eq!(e.code, Some(-32011));
    assert_eq!(e.data["missing"], json!(["pet.load"]));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.load").is_empty());
}

// ---------------------------------------------------------------- particle_types

#[tokio::test]
async fn types_lists_the_default_rows_grouped_by_array() {
    let host = Host::new("types-default");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_types", json!({})).await.unwrap();
    assert_eq!(p["count"], json!(6));
    let arrays = p["arrays"].as_array().unwrap();
    assert_eq!(arrays.len(), 9);
    assert_eq!(arrays[1]["key"], json!("m_Emitters"));
    assert_eq!(arrays[1]["count"], json!(1));
    assert_eq!(arrays[3]["key"], json!("m_Operators"));
    assert_eq!(arrays[3]["count"], json!(3));
    assert_eq!(arrays[6]["count"], json!(1));
    assert_eq!(arrays[7]["key"], json!("m_Children"));
    assert_eq!(arrays[7]["count"], json!(0));
    assert_eq!(p["functions"].as_array().unwrap().len(), 6);

    // the array filter keeps only that array's rows (a file key, not the type index)
    let filtered = server
        .call("particle_types", json!({"array": "m_Operators"}))
        .await
        .unwrap();
    let rows = filtered["functions"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r["type"] == json!(3)));

    // Negative: an unknown array key is refused before anything is sent
    let before = mb.method_calls("pet.functionTypes").len();
    let e = server
        .call("particle_types", json!({"array": "m_NotAnArray"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, Some(-32602));
    assert_eq!(
        mb.method_calls("pet.functionTypes").len(),
        before,
        "the refusal must not dispatch"
    );
}

#[tokio::test]
async fn types_write_snapshot_round_trips_through_the_schema_loader() {
    let host = Host::new("types-snapshot");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .game_build("2000930")
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_types", json!({"writeSnapshot": true}))
        .await
        .unwrap();
    let snapshot = host.snapshot();
    assert_eq!(p["snapshotPath"], json!(snapshot.display().to_string()));
    assert_eq!(p["build"], json!("2000930"));

    // the file loads as a real (non-placeholder) 0.3 snapshot
    let text = std::fs::read_to_string(&snapshot).unwrap();
    assert!(text.ends_with("}\n"), "trailing newline: {text:?}");
    let raw: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(raw["placeholder"], json!(false));
    assert_eq!(raw["build"], json!("2000930"));
    assert_eq!(raw["rootClass"], json!("CParticleSystemDefinition"));
    assert_eq!(raw["arrays"].as_array().unwrap().len(), 9);
    assert_eq!(raw["functions"].as_array().unwrap().len(), 6);
    assert_eq!(raw["arrays"][7]["elementClass"], json!("ParticleChildrenInfo_t"));

    let schema = Schema::load(&snapshot).expect("the written snapshot must load");
    assert_eq!(schema.build.as_deref(), Some("2000930"));
    assert_eq!(schema.arrays.len(), 9);
    assert_eq!(schema.functions.len(), 6);
    for class in [
        "C_OP_ContinuousEmitter",
        "C_INIT_CreateWithinSphereTransform",
        "C_OP_BasicMovement",
        "C_OP_Decay",
        "C_OP_RenderSprites",
        "C_OP_OldStyleObsolete",
        "CParticleSystemDefinition",
        "ParticleChildrenInfo_t",
        "ParticleNamedValueSource_t",
    ] {
        assert!(schema.class(class).is_some(), "{class} missing from the snapshot");
    }
    assert!(schema.is_obsolete(3, "C_OP_OldStyleObsolete"));
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn types_write_snapshot_aborts_when_a_schema_lookup_fails_and_writes_no_file() {
    let host = Host::new("types-snapshot-fail");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .error(
            "pet.schema",
            -32017,
            "PrerequisiteFailed",
            Some(json!({"reason": "particle_mgr_null"})),
        )
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_types", json!({"writeSnapshot": true}))
        .await
        .unwrap_err();
    assert_eq!(e.code, Some(-32017));
    assert_eq!(e.name, "PrerequisiteFailed");
    assert!(!host.snapshot().exists(), "no partial snapshot may be written");
}

#[tokio::test]
async fn types_write_snapshot_without_the_schema_capability_refuses() {
    let host = Host::new("types-snapshot-gated");
    // pet.functionTypes stays advertised but pet.schema disappears (a partial bridge): writeSnapshot
    // must fail closed rather than send an unadvertised method.
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    mb.set_methods(["pet.functionTypes"]);
    // refresh the client's capability cache (particle_status always re-fetches control.capabilities)
    let status = server.call("particle_status", json!({})).await.unwrap();
    assert!(status["capabilities"]["missing"]
        .as_array()
        .unwrap()
        .contains(&json!("pet.schema")));
    let e = server
        .call("particle_types", json!({"writeSnapshot": true}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "NotImplemented");
    assert_eq!(e.data["missing"], json!(["pet.schema"]));
    assert!(mb.method_calls("pet.schema").is_empty());
    assert!(!host.snapshot().exists());
}

// ---------------------------------------------------------------- particle_schema

#[tokio::test]
async fn schema_live_uses_the_running_pet() {
    let host = Host::new("schema-live");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_schema", json!({"class": "C_OP_ContinuousEmitter"}))
        .await
        .unwrap();
    assert_eq!(p["source"], json!("live"));
    assert_eq!(p["found"], json!(true));
    assert_eq!(p["class"], json!("C_OP_ContinuousEmitter"));
    assert!(p["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == json!("m_flEmitRate")));
    assert!(p["bases"]
        .as_array()
        .unwrap()
        .contains(&json!("CParticleFunction")));
    // Negative: an unknown class is found:false (a result, not an error)
    let missing = server
        .call("particle_schema", json!({"class": "NoSuchClass_x"}))
        .await
        .unwrap();
    assert_eq!(missing["source"], json!("live"));
    assert_eq!(missing["found"], json!(false));
    assert_eq!(missing["fields"], json!([]));
}

#[tokio::test]
async fn schema_without_the_module_uses_the_local_snapshot() {
    let host = Host::new("schema-snapshot");
    host.write_snapshot();
    // no pet.* advertised at all: the tool must answer from the snapshot, never probe
    let mb = MockBridge::builder().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_schema", json!({"class": "C_OP_ContinuousEmitter"}))
        .await
        .unwrap();
    assert_eq!(p["source"], json!("snapshot"));
    assert_eq!(p["found"], json!(true));
    assert!(p["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == json!("m_flEmitRate")));
    assert!(p["bases"]
        .as_array()
        .unwrap()
        .contains(&json!("CParticleFunctionEmitter")));
    let missing = server
        .call("particle_schema", json!({"class": "NoSuchClass_x"}))
        .await
        .unwrap();
    assert_eq!(missing["source"], json!("snapshot"));
    assert_eq!(missing["found"], json!(false));
    // Negative: a class name with anything but [A-Za-z0-9_] is refused locally
    let e = server
        .call("particle_schema", json!({"class": "C_OP_Bad-Dash"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, Some(-32602));
    assert!(mb.method_calls("pet.schema").is_empty());
}

// ---------------------------------------------------------------- particle_open

#[tokio::test]
async fn open_happy_path_sends_the_addon_relative_path() {
    let host = Host::new("open-happy");
    host.write_probe();
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call(
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"}),
        )
        .await
        .unwrap();
    assert_eq!(p["opened"], json!(true));
    assert_eq!(p["path"], json!("csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"));
    assert_eq!(p["document"]["path"], json!(DOC_WIN));
    assert_eq!(p["document"]["inAddon"], json!(true));
    assert_eq!(
        p["document"]["arrays"]["m_Operators"],
        json!(["C_OP_BasicMovement", "C_OP_Decay"])
    );
    let calls = mb.method_calls("pet.open");
    assert_eq!(calls.len(), 1);
    // pet's OpenFile resolves relative to the addon content root
    assert_eq!(
        calls[0].params["path"],
        json!("particles/mcp_probe/probe.vpcf")
    );
    assert!(pet_deadlines(&mb).iter().all(|d| *d > 0));
}

#[tokio::test]
async fn open_refuses_an_unknown_class_and_sends_nothing() {
    let host = Host::new("open-unknown-class");
    host.write_snapshot();
    host.write_particle(
        "particles/mcp_probe/bad_class.vpcf",
        &PROBE.replace("C_OP_Decay", "C_OP_NotAThing"),
    );
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call(
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/bad_class.vpcf"}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert!(e.retry_safe);
    let issues = e.data["issues"].as_array().unwrap();
    assert!(
        issues.iter().any(|i| i["path"] == json!("m_Operators/1")),
        "{issues:?}"
    );
    assert!(mb.method_calls("pet.open").is_empty());
    // the document was resolved first (the new order: pet.active + dirty check, then validate)
    assert_eq!(mb.method_calls("pet.active").len(), 1);
}

#[tokio::test]
async fn open_refuses_a_non_kv3_file_and_sends_nothing() {
    let host = Host::new("open-broken");
    host.write_snapshot();
    host.write_particle("particles/mcp_probe/broken.vpcf", "{{{ not kv3");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call(
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/broken.vpcf"}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed");
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.open").is_empty());
}

#[tokio::test]
async fn open_refuses_a_dirty_document_then_force_proceeds() {
    let host = Host::new("open-dirty");
    host.write_probe();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, true)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let args = json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"});
    let e = server.call("particle_open", args.clone()).await.unwrap_err();
    assert_eq!(e.name, "UnsavedChanges");
    assert_eq!(e.code, Some(-32016));
    assert!(mb.method_calls("pet.open").is_empty());
    let mut forced = args;
    forced["force"] = json!(true);
    let p = server.call("particle_open", forced).await.unwrap();
    assert_eq!(p["opened"], json!(true));
    assert_eq!(mb.method_calls("pet.open").len(), 1);
}

/// NEGATIVE (TOCTOU order): the dirty check runs under the bridge lock and the validation read
/// is the last step before the send, so a dirty document is refused **before** a validation
/// refusal, and a clean one is only validated right in front of `pet.open`. Proven through the
/// `method_calls` order: `pet.active` (the dirty check) precedes the validation outcome, and a
/// failing validation still dispatches nothing.
#[tokio::test]
async fn open_validates_after_the_dirty_check_and_right_before_the_send() {
    let host = Host::new("open-toctou");
    host.write_snapshot();
    host.write_particle("particles/mcp_probe/broken.vpcf", "{{{ not kv3");
    let args = json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/broken.vpcf"});

    // a dirty live document: UnsavedChanges, not ValidationFailed (the dirty document was
    // resolved before the broken file was ever read)
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, true)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_open", args.clone()).await.unwrap_err();
    assert_eq!(e.name, "UnsavedChanges", "{e:?}");
    assert_eq!(e.code, Some(-32016));
    assert!(mb.method_calls("pet.open").is_empty());

    // a clean live document: `pet.active` precedes the validation outcome; the broken file is
    // refused with `pet.open` never called (nothing was sent)
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_open", args).await.unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert!(mb.method_calls("pet.open").is_empty(), "nothing was sent");
    assert!(
        mb.method_calls("pet.active").len() == 1
            && mb
                .requests()
                .iter()
                .filter(|r| r.method.starts_with("pet."))
                .all(|r| r.method == "pet.active"),
        "the validation ran after the one pet.active round trip, and nothing else pet.*: {requests:?}",
        requests = mb.requests()
    );
}

#[tokio::test]
async fn open_without_a_tree_schema_checks_the_built_in_and_skip_schema_stays_structural() {
    let host = Host::new("open-no-schema");
    // no snapshot in the temp tree: the committed live snapshot (build 2000930) is the fallback
    host.write_particle("particles/mcp_probe/probe.vpcf", PROBE);
    host.write_particle(
        "particles/mcp_probe/unknown.vpcf",
        &PROBE.replace("C_OP_Decay", "C_OP_NoSuchThing"),
    );
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let unknown = json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/unknown.vpcf"});
    let e = server.call("particle_open", unknown).await.unwrap_err();
    assert_eq!(e.name, "ValidationFailed");
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.open").is_empty(), "nothing sent for an unknown class");
    let args = json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"});
    let p = server.call("particle_open", args.clone()).await.unwrap();
    assert_eq!(p["opened"], json!(true));
    assert_eq!(mb.method_calls("pet.open").len(), 1);

    // skipSchema skips only the schema check; the structural checks still ran
    let mut skipped = args;
    skipped["skipSchema"] = json!(true);
    let p = server.call("particle_open", skipped).await.unwrap();
    assert_eq!(p["opened"], json!(true));

    // Negative: skipSchema never skips structural problems (a wrong root class)
    host.write_particle(
        "particles/mcp_probe/bad_root.vpcf",
        &PROBE.replace("CParticleSystemDefinition", "CParticleSomethingElse"),
    );
    let e = server
        .call(
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/bad_root.vpcf", "skipSchema": true}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed");
}

#[tokio::test]
async fn open_refuses_paths_outside_the_addon_and_off_the_addon_extension() {
    let host = Host::new("open-outside");
    let core = host.write_core_particle("particles/core.vpcf", PROBE);
    assert!(core.is_file());
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    for path in [
        "csgo/particles/core.vpcf",
        "csgo_addons/some_other_addon/particles/x.vpcf",
        "csgo_addons/mcp_test/particles/../core.vpcf",
        "csgo_addons/mcp_test/models/x.vmdl",
    ] {
        let e = server.call("particle_open", json!({"path": path})).await.unwrap_err();
        assert_eq!(e.code, Some(-32602), "{path}: {e:?}");
    }
    assert!(mb.method_calls("pet.open").is_empty());
}

// ---------------------------------------------------------------- particle_reload

#[tokio::test]
async fn reload_refuses_a_broken_live_file_and_sends_nothing() {
    let host = Host::new("reload-broken");
    host.write_probe();
    // the live document's file is overwritten with garbage (what a failed load leaves behind)
    host.write_particle("particles/mcp_probe/probe.vpcf", "{{{ not kv3");
    let doc = make_pet_doc(DOC_WIN, false);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert_eq!(e.code, Some(-32602));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.reload").is_empty());
    // the check ran: the document was resolved first
    assert_eq!(mb.method_calls("pet.active").len(), 1);
}

#[tokio::test]
async fn reload_refuses_a_dirty_document_without_force() {
    let host = Host::new("reload-dirty");
    host.write_probe();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, true)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    assert_eq!(e.name, "UnsavedChanges");
    assert_eq!(e.code, Some(-32016));
    assert!(mb.method_calls("pet.reload").is_empty());

    let p = server.call("particle_reload", json!({"force": true})).await.unwrap();
    assert_eq!(p["reloaded"], json!(true));
    assert_eq!(p["hasRoot"], json!(true));
    assert_eq!(p["discardedChanges"], json!(true));
    assert_eq!(p["path"], json!(DOC_WIN));
    assert_eq!(mb.method_calls("pet.reload").len(), 1);
    assert_eq!(mb.method_calls("pet.reload")[0].params["force"], json!(true));
}

#[tokio::test]
async fn reload_without_a_document_is_no_active_document() {
    let host = Host::new("reload-nodoc");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    assert_eq!(e.code, Some(-32003));
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    assert!(mb.method_calls("pet.reload").is_empty());
}

#[tokio::test]
async fn reload_refuses_an_unmapped_document_path() {
    let host = Host::new("reload-unmapped");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc("q:\\nowhere\\x.vpcf", false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    assert_eq!(e.name, "PathNotMapped");
    assert_eq!(e.code, Some(-32602));
    assert!(mb.method_calls("pet.reload").is_empty());
}

/// NEGATIVE (TOCTOU order, same shape as open): the dirty check precedes the validation read.
#[tokio::test]
async fn reload_validates_after_the_dirty_check() {
    let host = Host::new("reload-toctou");
    host.write_snapshot();
    // the live document's file is broken on disk
    host.write_particle("particles/mcp_probe/probe.vpcf", "{{{ not kv3");

    // a dirty live document: UnsavedChanges, not ValidationFailed — the dirty check ran first
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, true)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    assert_eq!(e.name, "UnsavedChanges", "{e:?}");
    assert_eq!(e.code, Some(-32016));
    assert!(mb.method_calls("pet.reload").is_empty());

    // forced past the dirty check, the broken file still refuses before anything is sent
    let e = server
        .call("particle_reload", json!({"force": true}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "ValidationFailed", "{e:?}");
    assert!(mb.method_calls("pet.reload").is_empty(), "nothing was sent");
    // two pet.active round trips (one per call) preceded each validation outcome
    assert_eq!(mb.method_calls("pet.active").len(), 2);
}

/// NEGATIVE: the bridge's own lex refusal (`-32602 UnloadableFile {reason, path}`) surfaces
/// through the tool exactly as the bridge reported it, even when the host file is perfectly
/// valid (so the Rust guard passed and the request was dispatched).
#[tokio::test]
async fn reload_surfaces_the_bridge_unloadable_refusal() {
    let host = Host::new("reload-unloadable");
    host.write_probe();
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .pet_unloadable(Some("unbalanced"))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_reload", json!({})).await.unwrap_err();
    // what `ToolError::from(BridgeError)` produces for `{"code":-32602,"message":
    // "UnloadableFile","data":{"reason":"unbalanced","path":…}}`: the wire code, the bridge's
    // message name, and `data.reason`/`data.path` carried through
    assert_eq!(e.code, Some(-32602), "{e:?}");
    assert_eq!(e.wire_name.as_deref(), Some("UnloadableFile"));
    assert_eq!(e.data["reason"], json!("unbalanced"));
    assert_eq!(e.data["path"], json!(DOC_WIN));
    assert!(mb.method_calls("pet.reload").len() == 1, "the request was dispatched once (the refusal is the bridge's, not the tool's)");
}

// ---------------------------------------------------------------- particle_save

#[tokio::test]
async fn save_refuses_untitled_documents_and_sends_nothing() {
    let host = Host::new("save-untitled");
    let mut doc = make_pet_doc(DOC_WIN, false);
    doc["isNewFile"] = json!(true);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_save", json!({})).await.unwrap_err();
    assert_eq!(e.name, "UntitledDocument");
    assert_eq!(e.code, Some(-32015));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.save").is_empty());
}

#[tokio::test]
async fn save_refuses_paths_outside_the_addon() {
    for (tag, win) in [
        ("save-core", "Z:\\app\\content\\csgo\\particles\\core.vpcf"),
        ("save-unmapped", "q:\\nowhere\\x.vpcf"),
    ] {
        let host = Host::new(tag);
        let mb = MockBridge::builder()
            .pet_loaded(true)
            .pet_doc(Some(make_pet_doc(win, false)))
            .start()
            .await
            .unwrap();
        let server = Server::new(&mb.discovery_path, &host.dir);
        let e = server.call("particle_save", json!({})).await.unwrap_err();
        assert_eq!(e.name, "ReadOnlyParticle", "{tag}: {e:?}");
        assert!(e.retry_safe);
        assert!(mb.method_calls("pet.save").is_empty(), "{tag}");
    }
}

#[tokio::test]
async fn save_reports_skipped_clean_and_saves_a_dirty_document() {
    let host = Host::new("save-clean");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_save", json!({})).await.unwrap();
    assert_eq!(
        p,
        json!({"saved": true, "skippedClean": true, "forced": false, "path": DOC_WIN})
    );
    assert_eq!(mb.method_calls("pet.save").len(), 1);

    // the mock went clean; make it dirty again and save for real
    mb.with_state(|s| {
        if let Some(d) = s.pet_doc.as_mut() {
            d["dirty"] = json!(true);
        }
    });
    let p = server.call("particle_save", json!({})).await.unwrap();
    assert_eq!(p["skippedClean"], json!(false));
    mb.with_state(|s| assert_eq!(s.pet_doc.as_ref().unwrap()["dirty"], json!(false)));
}

#[tokio::test]
async fn save_force_saves_a_clean_document() {
    let host = Host::new("save-force");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server
        .call("particle_save", json!({"force": true}))
        .await
        .unwrap();
    // a clean document forced through pet: saved, re-serialized (never skippedClean)
    assert_eq!(
        p,
        json!({"saved": true, "skippedClean": false, "forced": true, "path": DOC_WIN})
    );
    let calls = mb.method_calls("pet.save");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].params["force"], json!(true));
}

/// NEGATIVE: without `force` a clean document stays skipped and the wire params carry no `force`
/// key at all (not `force:false`): a bridge that does not know the knob must see today's `{}`.
#[tokio::test]
async fn save_without_force_skips_a_clean_document_and_sends_no_force() {
    let host = Host::new("save-no-force");
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(make_pet_doc(DOC_WIN, false)))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let p = server.call("particle_save", json!({})).await.unwrap();
    assert_eq!(
        p,
        json!({"saved": true, "skippedClean": true, "forced": false, "path": DOC_WIN})
    );
    let calls = mb.method_calls("pet.save");
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].params.get("force").is_none(),
        "no force key may be sent without it: {}",
        calls[0].params
    );
}

/// NEGATIVE: `force` never unlocks an untitled document (the bridge would open Save As); nothing
/// is sent.
#[tokio::test]
async fn save_force_still_refuses_an_untitled_document() {
    let host = Host::new("save-force-untitled");
    let mut doc = make_pet_doc(DOC_WIN, false);
    doc["isNewFile"] = json!(true);
    let mb = MockBridge::builder()
        .pet_loaded(true)
        .pet_doc(Some(doc))
        .start()
        .await
        .unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server
        .call("particle_save", json!({"force": true}))
        .await
        .unwrap_err();
    assert_eq!(e.name, "UntitledDocument");
    assert_eq!(e.code, Some(-32015));
    assert!(e.retry_safe);
    assert!(mb.method_calls("pet.save").is_empty());
    // the document was resolved first (the refusal is the tool's own, before any save)
    assert_eq!(mb.method_calls("pet.active").len(), 1);
}

#[tokio::test]
async fn save_without_a_document_is_no_active_document() {
    let host = Host::new("save-nodoc");
    let mb = MockBridge::builder().pet_loaded(true).start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let e = server.call("particle_save", json!({})).await.unwrap_err();
    assert_eq!(e.code, Some(-32003));
    assert_eq!(e.wire_name.as_deref(), Some("NoActiveDocument"));
    assert!(mb.method_calls("pet.save").is_empty());
}

// ---------------------------------------------------------------- capability gating

#[tokio::test]
async fn gated_live_tools_without_the_module_answer_not_implemented_and_send_nothing() {
    let host = Host::new("gating");
    let mb = MockBridge::builder().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    for (tool, args, first_missing) in [
        (
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"}),
            "pet.open",
        ),
        ("particle_reload", json!({}), "pet.reload"),
        ("particle_save", json!({}), "pet.save"),
        ("particle_types", json!({}), "pet.functionTypes"),
    ] {
        let e = server.call(tool, args).await.unwrap_err();
        assert_eq!(e.name, "NotImplemented", "{tool}");
        assert_eq!(e.code, Some(-32011), "{tool}");
        assert_eq!(e.data["missing"].as_array().unwrap()[0], json!(first_missing), "{tool}");
        assert!(e.retry_safe, "{tool}");
    }
    for m in ["pet.open", "pet.reload", "pet.save", "pet.functionTypes"] {
        assert!(mb.method_calls(m).is_empty(), "{m}");
    }
}

/// Every pet request sent through the tools carried an explicit `deadlineMs > 0`.
#[tokio::test]
async fn every_pet_request_carries_an_explicit_deadline() {
    let host = Host::new("deadlines");
    host.write_probe();
    let mb = MockBridge::builder().pet().start().await.unwrap();
    let server = Server::new(&mb.discovery_path, &host.dir);
    let _ = server.call("particle_load", json!({})).await;
    let _ = server.call("particle_status", json!({})).await;
    let _ = server
        .call(
            "particle_open",
            json!({"path": "csgo_addons/mcp_test/particles/mcp_probe/probe.vpcf"}),
        )
        .await;
    let _ = server.call("particle_reload", json!({})).await;
    let _ = server.call("particle_save", json!({})).await;
    let _ = server.call("particle_types", json!({})).await;
    let _ = server.call("particle_schema", json!({"class": "C_OP_Decay"})).await;
    let deadlines = pet_deadlines(&mb);
    assert!(!deadlines.is_empty());
    assert!(deadlines.iter().all(|d| *d > 0), "{deadlines:?}");
}
