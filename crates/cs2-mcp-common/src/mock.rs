//! Mock bridge: the recorded wire contract on a real loopback socket (port of `mock_bridge.py`).
//!
//! Same protocol as the real bridge: NDJSON JSON-RPC 2.0 on an ephemeral loopback port, a
//! discovery file with a token, `control.hello` as the mandatory first frame, one authenticated
//! client at a time (`Busy` on accept for a second), the same envelope and error responses. The
//! baseline (discovery, capabilities, document, envelopes) comes from `fixtures/bridge-latest.jsonl`;
//! responses are synthesised so callers may ask in any order.
//!
//! Failure knobs (mock-only, never wire methods): `main_thread_age_ms` (stale -> `HammerBlocked`),
//! `delay` (> deadline -> `-32008 QueueTimeout`), `silent` (no reply -> `OutcomeUnknown`), `drop`
//! (close before replying -> `ConnectionLost`), `error` (inject any wire error), `responder`
//! (scripted results), `methods` (capability gating), `revision`, `jobs`, `pet_unloadable`
//! (open/reload/apply -> `-32602 UnloadableFile`, the bridge's lex refusal), `pet_render_fail`
//! (`pet.render` -> `-32020 RenderFailed`), `modeldoc_render_fail` (`modeldoc.render` ->
//! `-32020 RenderFailed`), `modeldoc_no_session` (`modeldoc.render` -> `-32019 NoViewport`
//! `no_session`), `view_render_fail` (`view.render` -> `-32019 NoViewport {reason}`),
//! `world_no_map` (`world.render` renders the empty scene world: a black image, review-2 L6) /
//! `world_no_scene_world` (`world.render` -> `-32021 NoGameWorld`, the rare case), `render_dir` (the Hammer render arms
//! write a tiny valid PNG under it and report its real `bytes` length).
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub mod jobs;
pub mod podman;
pub mod rcon;
pub mod stdio;

mod hammer;
pub use self::hammer::{
    HammerWorld, MeshTransform, MockConnection, MockMesh, MockNode, SelectionState, ENTITY_CLASSES,
    HAMMER_METHODS,
};

pub const DEFAULT_FIXTURES: &str = include_str!("../fixtures/bridge-latest.jsonl");
/// Synthetic m2.10 replies (`game.rules`, `game.entities`), tagged with `method`; not a live recording.
pub const SYNTHETIC_FIXTURES: &str = include_str!("../fixtures/synthetic-m2.10.jsonl");

/// The six ModelDoc bridge methods (m2.11, `crates/cs2-bridge/PROTOCOL.md` §16)
/// plus the m2.14 render method. Advertised by hello only when the mock was built with
/// [`MockBuilder::modeldoc`]; the six m2.11 built-ins answer regardless (tests decide per call),
/// `modeldoc.render` only when advertised (gated like `pet.*`). No `model_doc` = no session for
/// every arm (`modeldoc.active` reason `no_session`, `modeldoc.render` `NoViewport no_session`).
/// Addon the mock's `modeldoc.open` resolves paths under (the runtime starts with `-addon mcp_test`).
pub const MOCK_ADDON: &str = "mcp_test";

pub const MODELDOC_METHODS: [&str; 7] = [
    "modeldoc.active",
    "modeldoc.open",
    "modeldoc.reload",
    "modeldoc.save",
    "modeldoc.compile",
    "modeldoc.nodeTypes",
    "modeldoc.render",
];

/// The twelve `pet.*` bridge methods (m2.12 + the m2.13 second pass, plan2 §0.1/§0.3). Advertised
/// by hello only when the mock was built with [`MockBuilder::pet`] / [`MockBuilder::pet_loaded`];
/// the built-in responses below still answer them (like every other built-in), so tests can decide
/// per call.
pub const PET_METHODS: [&str; 12] = [
    "pet.load",
    "pet.active",
    "pet.open",
    "pet.reload",
    "pet.save",
    "pet.functionTypes",
    "pet.schema",
    "pet.text",
    "pet.apply",
    "pet.undo",
    "pet.redo",
    "pet.render",
];

/// The Hammer render methods (`view.render` m2.5 [V runtime] dead; `world.render` m2.5
/// [V runtime] dead): advertised in hello only when the mock was built with
/// [`MockBuilder::renders`], like `modeldoc()`/`pet()` (the real bridge answers
/// `-32011 NotImplemented` for an unregistered method and advertises what its prerequisites
/// allow; the fixture baseline has neither).
pub const RENDER_METHODS: [&str; 2] = ["view.render", "world.render"];

/// The eleven `NEED_PET` methods the mock advertises only after a fresh `pet.load` (before that,
/// only `pet.load` itself is advertised, mirroring the bridge's advertising of what its
/// prerequisites allow).
pub const PET_POST_LOAD_METHODS: [&str; 11] = [
    "pet.active",
    "pet.open",
    "pet.reload",
    "pet.save",
    "pet.functionTypes",
    "pet.schema",
    "pet.text",
    "pet.apply",
    "pet.undo",
    "pet.redo",
    "pet.render",
];

/// `pet.apply`/`pet.text`'s size gate (m2.13 §0.3: 384 KB of raw text, both directions).
pub const PET_TEXT_MAX: usize = 393216;

/// The document text `pet.text` serves by default (m2.13 §0.3): the live-compiled probe document
/// (`hammer-mcp/fixtures/vpcf/probe.vpcf`) with the template's `m_nBehaviorVersion = 12` line
/// after `_class`, CRLF endings, tabs, no final newline — pet's own serialization form.
pub const MOCK_PROBE_TEXT: &str = concat!(
    "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} ",
    "format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->\r\n",
    "{\r\n",
    "\t_class = \"CParticleSystemDefinition\"\r\n",
    "\tm_nBehaviorVersion = 12\r\n",
    "\tm_nMaxParticles = 32\r\n",
    "\tm_Emitters = \r\n",
    "\t[\r\n",
    "\t\t{\r\n",
    "\t\t\t_class = \"C_OP_ContinuousEmitter\"\r\n",
    "\t\t},\r\n",
    "\t]\r\n",
    "\tm_Initializers = \r\n",
    "\t[\r\n",
    "\t\t{\r\n",
    "\t\t\t_class = \"C_INIT_CreateWithinSphereTransform\"\r\n",
    "\t\t},\r\n",
    "\t]\r\n",
    "\tm_Operators = \r\n",
    "\t[\r\n",
    "\t\t{\r\n",
    "\t\t\t_class = \"C_OP_BasicMovement\"\r\n",
    "\t\t},\r\n",
    "\t\t{\r\n",
    "\t\t\t_class = \"C_OP_Decay\"\r\n",
    "\t\t},\r\n",
    "\t]\r\n",
    "\tm_Renderers = \r\n",
    "\t[\r\n",
    "\t\t{\r\n",
    "\t\t\t_class = \"C_OP_RenderSprites\"\r\n",
    "\t\t},\r\n",
    "\t]\r\n",
    "}",
);

/// Default `modeldoc.nodeTypes` class list (VRF-derived `template.rs` classes plus the live-snapshot
/// classes seen in the plan; the real list comes from the bridge snapshot, see
/// `hammer-mcp/research/modeldoc-nodetypes.json`). Order defines `typeId`.
pub const DEFAULT_NODE_TYPES: [&str; 18] = [
    "RootNode",
    "RenderMeshList",
    "RenderMeshFile",
    "PhysicsShapeList",
    "PhysicsHullFile",
    "MaterialGroupList",
    "DefaultMaterialGroup",
    "AnimationList",
    "AnimFile",
    "BoneMarkupList",
    "HitboxSetList",
    "HitboxSet",
    "AttachmentList",
    "LODGroupList",
    "LODGroup",
    "BreakPieceList",
    "BreakPieceEmbedded",
    "Mesh Operation Stack",
];

/// The recorded baseline plus the synthetic m2.10 replies the mock serves by default.
fn default_fixtures() -> Fixtures {
    let mut fx = Fixtures::parse(DEFAULT_FIXTURES);
    let synthetic = Fixtures::parse(SYNTHETIC_FIXTURES);
    fx.game_rules = synthetic.game_rules;
    fx.game_entities = synthetic.game_entities;
    fx
}
static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

pub type Responder = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Fixtures {
    pub discovery: Value,
    pub build: String,
    pub hello_result: Value,
    pub doc_result: Value,
    pub method_not_found: Value,
    pub invalid_request: Value,
    pub busy: Value,
    /// `game.rules` / `game.entities` replies recorded in the fixture (tagged `method`); served by default
    pub game_rules: Option<Value>,
    pub game_entities: Option<Value>,
}

impl Fixtures {
    /// Extract the baseline the mock serves from a `record.py`-style JSONL recording.
    pub fn parse(text: &str) -> Fixtures {
        let mut meta = Value::Null;
        let (mut hello, mut doc, mut not_found, mut id_less, mut busy) =
            (None, None, None, None, None);
        let (mut game_rules, mut game_entities) = (None, None);
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if rec.get("t").and_then(Value::as_str) == Some("meta")
                && rec.get("discovery").is_some()
            {
                meta = rec.clone();
            }
            if rec.get("t").and_then(Value::as_str) != Some("rx") {
                continue;
            }
            let Some(obj) = rec.get("obj").filter(|o| o.is_object()) else {
                continue;
            };
            let code = obj.pointer("/error/code").and_then(Value::as_i64);
            match rec.get("method").and_then(Value::as_str) {
                Some("game.rules") => game_rules = obj.get("result").cloned(),
                Some("game.entities") => game_entities = obj.get("result").cloned(),
                _ => {}
            }
            if hello.is_none() && obj.pointer("/result/capabilities").is_some() {
                hello = Some(obj.clone());
            } else if doc.is_none()
                && obj
                    .get("result")
                    .is_some_and(|r| r.is_object() && r.get("doc").is_some())
            {
                doc = Some(obj.clone());
            } else if code == Some(-32601) && not_found.is_none() {
                not_found = Some(obj.clone());
            } else if code == Some(-32600) && id_less.is_none() {
                id_less = Some(obj.clone());
            } else if code == Some(-32007) && busy.is_none() {
                busy = Some(obj.clone());
            }
        }
        let err_of = |o: &Option<Value>, dflt: Value| {
            o.as_ref()
                .and_then(|v| v.get("error").cloned())
                .unwrap_or(dflt)
        };
        Fixtures {
            discovery: meta.get("discovery").cloned().unwrap_or_else(|| json!({})),
            build: hello
                .as_ref()
                .and_then(|h| h.pointer("/bridge/build"))
                .and_then(Value::as_str)
                .unwrap_or("mock")
                .to_string(),
            hello_result: hello
                .as_ref()
                .and_then(|h| h.get("result").cloned())
                .unwrap_or_else(|| json!({})),
            doc_result: doc
                .as_ref()
                .and_then(|d| d.get("result").cloned())
                .unwrap_or_else(|| json!({"doc": null, "reason": "no_session"})),
            method_not_found: err_of(
                &not_found,
                json!({"code": -32601, "message": "MethodNotFound"}),
            ),
            invalid_request: err_of(
                &id_less,
                json!({"code": -32600, "message": "invalid request: id required"}),
            ),
            busy: err_of(&busy, json!({"code": -32007, "message": "Busy"})),
            game_rules,
            game_entities,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub params: Value,
    pub id: Value,
}

/// A wire error for `error_on_call`: the (1-based) call it applies to, then code, message and data.
pub type NthError = (usize, (i64, String, Option<Value>));

/// Mutable mock state shared with the serving tasks; tests mutate it through [`MockBridge`].
pub struct State {
    pub methods: Vec<String>,
    pub responders: HashMap<String, Responder>,
    pub errors: HashMap<String, (i64, String, Option<Value>)>,
    /// `error_on_call`: (nth call, error) per method; `calls` counts requests per method
    pub nth_errors: HashMap<String, NthError>,
    pub calls: HashMap<String, usize>,
    pub doc: Option<Value>,
    pub docs: Option<Vec<Value>>,
    /// ModelDoc: the active document (`modeldoc.active` / `modeldoc.open` / ...); `None` = no doc.
    pub model_doc: Option<Value>,
    /// ModelDoc: node-type class names in `typeId` order (`modeldoc.nodeTypes`).
    pub node_types: Vec<String>,
    /// pet (m2.12): `pet.dll` loaded (`pet.load` sets it; `pet.active`/`pet.functionTypes`
    /// answer per 0.2 regardless, like the built-ins).
    pub pet_loaded: bool,
    /// hammer.dll is not loaded yet ([`MockBridge::hammer_unloaded`]): `doc.active` answers the
    /// real bridge's `-32017 PrerequisiteFailed {module:"hammer.dll", state:"not_loaded"}`.
    pub hammer_not_loaded: bool,
    /// pet: the active particle document (`pet.active` / `pet.open` / ...); `None` = no doc.
    pub pet_doc: Option<Value>,
    /// pet: `pet.functionTypes` rows (0.2 `types[]` shape).
    pub pet_types: Vec<Value>,
    /// pet: `pet.schema` replies per class name.
    pub pet_schemas: HashMap<String, Value>,
    /// pet: when set, `pet.open`/`pet.reload`/`pet.apply` answer the bridge's `-32602
    /// UnloadableFile` `{reason, path}` (the lex-refusal ReloadDocument/OpenFile/vpcf_check raise
    /// without touching the document) instead of running their built-in semantics. The value is
    /// the `data.reason`.
    pub pet_unloadable: Option<String>,
    /// pet (m2.13): the document text `pet.text` serves and `pet.apply` overwrites.
    pub pet_text: String,
    /// pet (m2.13): undo stack of previous document texts (`pet.apply` pushes, `pet.text`
    /// derives `canUndo` from it, `pet.undo` pops).
    pub pet_undo: Vec<String>,
    /// pet (m2.13): redo stack of undone document texts (`pet.undo` pushes, `pet.redo` pops).
    pub pet_redo: Vec<String>,
    /// pet (m2.13): when set, `pet.render` answers `-32020 RenderFailed {reason}` instead of
    /// its built-in result. The value is the `data.reason`.
    pub pet_render_error: Option<String>,
    /// pet (m2.13): `pet.render`'s default-stem sequence (`render_<seq>` when no name is given).
    pub pet_render_seq: u32,
    /// ModelDoc (m2.14): when set, `modeldoc.render` answers `-32020 RenderFailed {reason}`
    /// instead of its built-in result. The value is the `data.reason`.
    pub modeldoc_render_error: Option<String>,
    /// ModelDoc (m2.14): `modeldoc.render`'s default-stem sequence (`render_<seq>` when no name
    /// is given).
    pub modeldoc_render_seq: u32,
    /// ModelDoc (m2.14): when set, `modeldoc.render` answers `-32019 NoViewport` `no_session`
    /// (no active session on the modeldoc chain; the mock cannot reach the real per-hop reasons).
    pub modeldoc_no_session: bool,
    /// The Hammer renders (WP-D): when set, `view.render` answers `-32019 NoViewport {reason}`
    /// (the bridge's per-hop refusal: `no_view_manager` / `no_3d_view` / `bad_view`; the mock
    /// cannot inspect the real object chain, so it reports the reason verbatim).
    pub view_render_fail: Option<String>,
    /// The Hammer renders (WP-D): when set, `world.render` answers a **uniform-black image**
    /// (review-2 L6 live behaviour: no map loaded in the engine, e.g. after `disconnect`, still
    /// renders the empty scene world) — all-zero stats plus the black PNG. `world_no_scene_world`
    /// is the error case.
    pub world_no_map: bool,
    /// The Hammer renders (WP-D): when set, `world.render` answers
    /// `-32021 NoGameWorld {reason: "no map is loaded in the engine (map <name>)"}` — the rare
    /// case the live bridge answers only when `GetGameSceneWorld` is null outright.
    pub world_no_scene_world: bool,
    /// The Hammer renders (WP-D): when set, `view.render`/`world.render` write the tiny valid
    /// PNG (`RgbImage::to_png`, 8x4 solid colour like the POC `support.make_png`) under this
    /// host directory (mapped from `Z:\app\renders\<file>`) and report its real length in
    /// `bytes`; when unset they report `bytes: 0` and write nothing (the `pet.render` /
    /// `modeldoc.render` shape, where the tests write the file themselves).
    pub render_dir: Option<PathBuf>,
    /// The Hammer renders (WP-D): the default-stem sequence (`render_<seq>` when no `name` is
    /// given), per method like `pet_render_seq`/`modeldoc_render_seq`.
    pub view_render_seq: u32,
    pub world_render_seq: u32,
    /// The Hammer entity world (WP-M): built-ins serve the twenty [`HAMMER_METHODS`] when set
    /// (enabled + advertised by [`MockBuilder::hammer_nodes`]); `None` = the fixture baseline.
    pub hammer: Option<HammerWorld>,
    pub main_thread_age_ms: Option<u64>,
    pub drain_blocked: bool,
    pub queue_depth: i64,
    pub shutting_down: bool,
    pub inflight: i64,
    pub revision: Option<i64>,
    pub delay_methods: HashMap<String, u64>,
    pub silent_methods: HashSet<String>,
    pub drop_methods: HashSet<String>,
    pub jobs: Vec<Value>,
    /// The bridge's `job.*` model ([`jobs::MockJobs`]; advertised by [`MockBuilder::jobs`]).
    pub bridge_jobs: jobs::MockJobs,
    pub requests: Vec<RecordedRequest>,
    pub authed: bool,
    pub client_count: u64,
    pub busy_count: u64,
}

pub struct MockBuilder {
    fixtures: Fixtures,
    host: String,
    token: Option<String>,
    build: Option<String>,
    game_build: Option<String>,
    protocol: u64,
    methods: Option<Vec<String>>,
    responders: HashMap<String, Responder>,
    errors: HashMap<String, (i64, String, Option<Value>)>,
    nth_errors: HashMap<String, NthError>,
    doc: Option<Option<Value>>,
    docs: Option<Vec<Value>>,
    model_doc: Option<Option<Value>>,
    node_types: Option<Vec<String>>,
    modeldoc: bool,
    /// pet (m2.12): `pet_loaded(true)` advertises the twelve `pet.*` methods from the start;
    /// `pet()` advertises only `pet.load` until a fresh load adds the eleven NEED_PET ones.
    pet: bool,
    pet_loaded: bool,
    pet_doc: Option<Option<Value>>,
    pet_types: Option<Vec<Value>>,
    pet_schemas_manual: HashMap<String, Value>,
    pet_unloadable: Option<String>,
    pet_text: Option<String>,
    pet_render_fail: Option<String>,
    modeldoc_render_fail: Option<String>,
    modeldoc_no_session: bool,
    /// WP-D: `None` = `view.render` runs its built-in; `Some(reason)` answers
    /// `-32019 NoViewport {reason}`.
    view_render_fail: Option<String>,
    /// WP-D: `true` = `world.render` renders the empty (black) scene world (review-2 L6).
    world_no_map: bool,
    /// WP-D: `true` = `world.render` answers `-32021 NoGameWorld {reason: "no map is loaded
    /// in the engine (map <name>)"}` (the rare `GetGameSceneWorld == null` case).
    world_no_scene_world: bool,
    /// WP-D: when set, the `view.render`/`world.render` arms write the tiny valid PNG under it
    /// (mapped from `Z:\app\renders`) and report its length; leaves it unwritten otherwise.
    render_dir: Option<PathBuf>,
    /// WP-D: advertise [`RENDER_METHODS`] when the mock was built with `.renders()`.
    pub renders: bool,
    /// WP-M: `Some(nodes)` enables the hammer entity world (advertises [`HAMMER_METHODS`]).
    hammer_nodes: Option<Vec<MockNode>>,
    /// WP-M: `map.save` answers `-32015 UntitledDocument` while set (doc `isNewFile`).
    hammer_new_file: bool,
    /// WP-M: mutators answer `-32018 DocumentNotEditable` while false (doc `editable`).
    hammer_editable: bool,
    /// M1 knob (mesh review): faces Hammer "welds away" from every geometry request.
    hammer_drop_faces: usize,
    /// W2 #3 seam: the entry index whose add read-back fails (`None` = every entry confirms).
    hammer_drop_conn_readback: Option<i64>,
    /// m2.18 (review w2b #5): the hammer world's value-undo modelling — `true` (the default)
    /// is the m2.18 build: `valueUndo:"recorded"`, the popped `"MCP: set keyvalues"` group
    /// restores the values, and `node.setTransform` is advertised. `false` is a pre-m2.18
    /// build: the reply says `"group-only"`, the pop restores nothing, and the advertisement
    /// drops `node.setTransform` (an m2.18-advertising build never answers `"group-only"`).
    hammer_value_undo_recorded: bool,
    /// m3.12: advertise and serve `selection.get` / `selection.set` (the hammer world's selection
    /// model); `false` (the default) is a bridge without them.
    hammer_selection: bool,
    /// m3.12: whether the hammer world serves and advertises `mesh.get` (default `true`).
    hammer_mesh_get: bool,
    hammer_face_texture: bool,
    /// m3.12: `mesh.get` answers the "too large for one reply" refusal for every mesh.
    hammer_mesh_too_large: bool,
    /// WP-OM: the addon-relative map files the mock's host has (`map.open`'s host gate).
    hammer_map_files: Vec<String>,
    /// advertise `job.start`/`job.status`/`job.cancel` ([`jobs`])
    bridge_jobs: bool,
    job_scripts: Vec<jobs::MockJobScript>,
    job_unavailable: Option<String>,
    job_writes: Vec<PathBuf>,
    job_content_root: Option<PathBuf>,
    main_thread_age_ms: Option<u64>,
    drain_blocked: bool,
    queue_depth: i64,
    shutting_down: bool,
    inflight: i64,
    revision: Option<i64>,
    delay_methods: HashMap<String, u64>,
    silent_methods: HashSet<String>,
    drop_methods: HashSet<String>,
    hello_timeout: Duration,
    discovery_dir: Option<PathBuf>,
}

impl MockBuilder {
    pub fn fixtures(mut self, text: &str) -> Self {
        self.fixtures = Fixtures::parse(text);
        self
    }
    pub fn token(mut self, t: impl Into<String>) -> Self {
        self.token = Some(t.into());
        self
    }
    pub fn build(mut self, b: impl Into<String>) -> Self {
        self.build = Some(b.into());
        self
    }
    pub fn game_build(mut self, b: impl Into<String>) -> Self {
        self.game_build = Some(b.into());
        self
    }
    pub fn protocol(mut self, p: u64) -> Self {
        self.protocol = p;
        self
    }
    /// Capability override (default: the recorded list + responder/error methods).
    pub fn methods<I: IntoIterator<Item = T>, T: Into<String>>(mut self, m: I) -> Self {
        self.methods = Some(m.into_iter().map(Into::into).collect());
        self
    }
    pub fn responder<F>(mut self, method: &str, f: F) -> Self
    where
        F: Fn(&Value) -> Value + Send + Sync + 'static,
    {
        self.responders.insert(method.to_string(), Arc::new(f));
        self
    }
    pub fn responder_value(self, method: &str, v: Value) -> Self {
        self.responder(method, move |_| v.clone())
    }
    /// Fail only the `nth` (1-based) request to `method` with the given wire error.
    pub fn error_on_call(
        mut self,
        method: &str,
        nth: usize,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Self {
        self.nth_errors
            .insert(method.to_string(), (nth, (code, message.to_string(), data)));
        self
    }
    pub fn error(mut self, method: &str, code: i64, message: &str, data: Option<Value>) -> Self {
        self.errors
            .insert(method.to_string(), (code, message.to_string(), data));
        self
    }
    /// `None` = serve `doc.active` as `{doc:null, reason:"no_session"}`.
    pub fn doc(mut self, d: Option<Value>) -> Self {
        self.doc = Some(d);
        self
    }
    pub fn docs(mut self, d: Vec<Value>) -> Self {
        self.docs = Some(d);
        self
    }
    /// Advertise the six `modeldoc.*` methods in hello's `capabilities.methods` (appended when
    /// they are not already present). The built-in ModelDoc semantics answer regardless, like
    /// every other built-in; this flag is what tests of capability gating look at.
    pub fn modeldoc(mut self) -> Self {
        self.modeldoc = true;
        self
    }
    /// Advertise only `pet.load` in hello (the fresh-load state). A `pet.load` adds the eleven
    /// `NEED_PET` methods and bumps the revision, like the bridge advertising its prerequisites.
    pub fn pet(mut self) -> Self {
        self.pet = true;
        self
    }
    /// Advertise all twelve `pet.*` methods from the start (`pet.dll` already loaded).
    pub fn pet_loaded(mut self, v: bool) -> Self {
        self.pet_loaded = v;
        self
    }
    /// The pet document `pet.active` serves (`None` = no document). Set per method call later
    /// through [`MockBridge::set_pet_doc`].
    pub fn pet_doc(mut self, d: Option<Value>) -> Self {
        self.pet_doc = Some(d);
        self
    }
    /// Rows `pet.functionTypes` serves (0.2 `types[]` shape). Default: [`default_pet_types`].
    pub fn pet_types(mut self, t: Vec<Value>) -> Self {
        self.pet_types = Some(t);
        self
    }
    /// A `pet.schema` reply for one class name (overrides the built-in defaults).
    pub fn pet_schema(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.pet_schemas_manual.insert(name.into(), schema);
        self
    }
    /// `Some(reason)`: `pet.open`/`pet.reload`/`pet.apply` answer the bridge's lex refusal
    /// `-32602 UnloadableFile {reason, path}` (open/reload: raised without touching the
    /// document, path = the file; apply: path = `pet.apply:text`) instead of running their
    /// built-in semantics. The value is the `data.reason`.
    pub fn pet_unloadable(mut self, reason: Option<impl Into<String>>) -> Self {
        self.pet_unloadable = reason.map(Into::into);
        self
    }
    /// The document text `pet.text` serves and `pet.apply` overwrites
    /// (default: [`MOCK_PROBE_TEXT`]).
    pub fn pet_text(mut self, t: impl Into<String>) -> Self {
        self.pet_text = Some(t.into());
        self
    }
    /// `Some(reason)`: `pet.render` answers the bridge's `-32020 RenderFailed {reason}`
    /// instead of its built-in result; `None` (default) runs the built-in.
    pub fn pet_render_fail(mut self, reason: Option<impl Into<String>>) -> Self {
        self.pet_render_fail = reason.map(Into::into);
        self
    }
    /// `Some(reason)`: `modeldoc.render` answers the bridge's `-32020 RenderFailed {reason}`
    /// instead of its built-in result; `None` (default) runs the built-in.
    pub fn modeldoc_render_fail(mut self, reason: Option<impl Into<String>>) -> Self {
        self.modeldoc_render_fail = reason.map(Into::into);
        self
    }
    /// `true`: `modeldoc.render` answers `-32019 NoViewport {"reason": "no_session"}` (no active
    /// session on the modeldoc chain) instead of its built-in result.
    pub fn modeldoc_no_session(mut self, v: bool) -> Self {
        self.modeldoc_no_session = v;
        self
    }
    /// Advertise the two Hammer render methods (`view.render`, `world.render`, WP-D) in hello's
    /// `capabilities.methods` (appended when not already present). The built-in semantics answer
    /// regardless, like every other built-in; the fixture baseline has neither, matching the
    /// recorded m2.8 set.
    pub fn renders(mut self) -> Self {
        self.renders = true;
        self
    }
    /// `Some(reason)`: `view.render` answers `-32019 NoViewport {reason}` (the per-hop chain
    /// refusal; the mock cannot inspect the real object chain) instead of its built-in result.
    pub fn view_render_fail(mut self, reason: Option<impl Into<String>>) -> Self {
        self.view_render_fail = reason.map(Into::into);
        self
    }
    /// `true`: `world.render` renders the empty (black) scene world — the render *succeeds*
    /// with all-black stats plus the blank note (review-2 L6's live no-map behaviour). The
    /// error case is `world_no_scene_world(true)`.
    pub fn world_no_map(mut self, v: bool) -> Self {
        self.world_no_map = v;
        self
    }
    /// `true`: `world.render` answers `-32021 NoGameWorld {"reason": "no map is loaded in the
    /// engine (map <name>)"}` (the rare `GetGameSceneWorld == null` case, review-2 L6).
    pub fn world_no_scene_world(mut self, v: bool) -> Self {
        self.world_no_scene_world = v;
        self
    }
    /// Host directory the `view.render`/`world.render` arms write the tiny valid PNG to
    /// (`Z:\app\renders\<file>` reaches `<dir>\<file>`); the arm then reports `bytes` = the
    /// file's real length. Unset = nothing is written and `bytes: 0` (tests write the PNG).
    pub fn render_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.render_dir = Some(dir.into());
        self
    }
    /// The ModelDoc document `modeldoc.active` serves (`None` = no document). Set per method call
    /// later through [`MockBridge::set_model_doc`].
    pub fn model_doc(mut self, d: Option<Value>) -> Self {
        self.model_doc = Some(d);
        self
    }
    /// Class list `modeldoc.nodeTypes` serves (`typeId` = index). Default: [`DEFAULT_NODE_TYPES`].
    pub fn node_types(mut self, t: Vec<String>) -> Self {
        self.node_types = Some(t);
        self
    }
    /// `None` omits `mainThreadAgeMs` from `control.ping` (liveness unknown).
    pub fn main_thread_age_ms(mut self, v: Option<u64>) -> Self {
        self.main_thread_age_ms = v;
        self
    }
    pub fn drain_blocked(mut self, v: bool) -> Self {
        self.drain_blocked = v;
        self
    }
    pub fn inflight(mut self, v: i64) -> Self {
        self.inflight = v;
        self
    }
    pub fn revision(mut self, v: i64) -> Self {
        self.revision = Some(v);
        self
    }
    pub fn shutting_down(mut self, v: bool) -> Self {
        self.shutting_down = v;
        self
    }
    pub fn delay(mut self, method: &str, ms: u64) -> Self {
        self.delay_methods.insert(method.to_string(), ms);
        self
    }
    pub fn silent(mut self, method: &str) -> Self {
        self.silent_methods.insert(method.to_string());
        self
    }
    pub fn drop_method(mut self, method: &str) -> Self {
        self.drop_methods.insert(method.to_string());
        self
    }
    /// Advertise the bridge's `job.*` methods and serve them ([`jobs`]); the outcomes are scripted
    /// with [`MockBuilder::job_script`] / [`MockBridge::script_job`].
    pub fn jobs(mut self) -> Self {
        self.bridge_jobs = true;
        self
    }
    /// Script one job's outcome (consumed in order by `job.start`; the default is `Ok` after one
    /// `running` answer). Implies [`MockBuilder::jobs`].
    pub fn job_script(mut self, outcome: jobs::MockJobOutcome, polls: u32) -> Self {
        self.bridge_jobs = true;
        self.job_scripts.push(jobs::MockJobScript { outcome, polls });
        self
    }
    /// The host files the mock compiler writes when a job turns `ok` (the compiled output).
    pub fn job_writes<I: IntoIterator<Item = PathBuf>>(mut self, paths: I) -> Self {
        self.bridge_jobs = true;
        self.job_writes.extend(paths);
        self
    }
    /// The content root `job.start` checks `input` against (the bridge's "input does not exist
    /// under the content root"); without it the mock does not check existence.
    pub fn job_content_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.bridge_jobs = true;
        self.job_content_root = Some(root.into());
        self
    }
    /// `job.start` answers `-32017 {blockedOn}` (e.g. `job: resourcecompiler is Windows-only`).
    pub fn job_unavailable(mut self, blocked_on: impl Into<String>) -> Self {
        self.bridge_jobs = true;
        self.job_unavailable = Some(blocked_on.into());
        self
    }
    pub fn hello_timeout(mut self, d: Duration) -> Self {
        self.hello_timeout = d;
        self
    }
    pub fn discovery_dir(mut self, p: PathBuf) -> Self {
        self.discovery_dir = Some(p);
        self
    }

    /// Enable the mock's Hammer entity world (WP-M): the twenty [`HAMMER_METHODS`] are advertised
    /// in hello and their built-ins serve the entity/node/undo/save semantics. The world's doc
    /// path/dirty are seeded from the document given to [`MockBuilder::doc`] (the fixture
    /// baseline doc otherwise defaults to the recorded probe path, so `map.save` works without
    /// an explicit `.doc(...)`).
    pub fn hammer_nodes(mut self, nodes: Vec<MockNode>) -> Self {
        self.hammer_nodes = Some(nodes);
        self
    }
    /// WP-M: the hammer document is an untitled new file (doc `isNewFile:true`,
    /// `map.save` answers `-32015 UntitledDocument`).
    pub fn hammer_new_file(mut self, v: bool) -> Self {
        self.hammer_new_file = v;
        self
    }
    /// WP-M: the hammer document is not editable (doc `editable:false`,
    /// every mutator answers `-32018 DocumentNotEditable`).
    pub fn hammer_editable(mut self, v: bool) -> Self {
        self.hammer_editable = v;
        self
    }
    /// M1 knob (mesh review): Hammer "welds away" up to `drop` faces of every
    /// `mesh.create`/`mesh.setGeometry` request — the reply's `faces` and the stored geometry both
    /// lose them, so the live face count no longer matches the request's (the mock's stand-in for a
    /// real weld dropping a degenerate polygon). Set per call through
    /// [`MockBridge::set_hammer_drop_faces`] as well.
    pub fn hammer_drop_faces(mut self, drop: usize) -> Self {
        self.hammer_drop_faces = drop;
        self
    }
    /// review-2 W2 #3 test seam: the read-back of the `at`-th (0-based) `entity.addConnections`
    /// wire entry fails (`{"readback":false,...}` in the reply, the bridge's maybe-executed case).
    /// `None` confirms every entry; see [`MockBridge::set_hammer_drop_conn_readback`] as well.
    pub fn hammer_drop_conn_readback(mut self, at: Option<i64>) -> Self {
        self.hammer_drop_conn_readback = at;
        self
    }

    /// m3.12: `false` models a bridge older than m3.12 — `mesh.get` is neither advertised nor
    /// served (hammer-mcp then falls back to the saved-file route).
    pub fn hammer_mesh_get(mut self, v: bool) -> Self {
        self.hammer_mesh_get = v;
        self
    }

    /// m3.14: `false` models a bridge older than m3.14 - `mesh.setFaceTexture` is neither
    /// advertised nor served.
    pub fn hammer_face_texture(mut self, v: bool) -> Self {
        self.hammer_face_texture = v;
        self
    }

    /// m3.12: `true` makes `mesh.get` refuse every mesh with the bridge's `-32602 "mesh too large
    /// for one reply"` (counts and `maxLineBytes` in `data`).
    pub fn hammer_mesh_too_large(mut self, v: bool) -> Self {
        self.hammer_mesh_too_large = v;
        self
    }

    /// m2.18 (review w2b #5): `false` models a pre-m2.18 build — the hammer reply says
    /// `valueUndo:"group-only"`, the popped group restores nothing, and the advertised
    /// capability set drops the m2.18 marker `node.setTransform`. The default is the m2.18
    /// shape (recorded + marker advertised).
    pub fn hammer_value_undo(mut self, v: bool) -> Self {
        self.hammer_value_undo_recorded = v;
        self
    }
    /// m3.12: model a bridge that has `selection.get` / `selection.set` (advertised and served
    /// from the hammer world's selection model; needs `.hammer_nodes`).
    pub fn hammer_selection(mut self, v: bool) -> Self {
        self.hammer_selection = v;
        self
    }
    /// WP-OM: addon-relative paths the mock's host has, on top of the active document's own file
    /// (`map.open` refuses anything else with `-32014 AssetNotFound`). `maps/b.vmap` etc.
    pub fn hammer_map_files<I, S>(mut self, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hammer_map_files = files.into_iter().map(Into::into).collect();
        self
    }

    pub async fn start(self) -> std::io::Result<MockBridge> {
        let fx = self.fixtures;
        let disc = fx.discovery.clone();
        let token = self
            .token
            .or_else(|| {
                disc.get("token")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "ab".repeat(32));
        let build = self.build.unwrap_or_else(|| fx.build.clone());
        let game_build = self
            .game_build
            .or_else(|| {
                disc.get("gameBuild")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "2000921".into());
        let base_methods: Vec<String> = fx
            .hello_result
            .pointer("/capabilities/methods")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut methods = self.methods.unwrap_or_else(|| {
            let mut m = base_methods.clone();
            let mut extra: Vec<String> = self
                .responders
                .keys()
                .chain(self.errors.keys())
                .cloned()
                .collect();
            extra.sort();
            extra.dedup();
            for e in extra {
                if !m.contains(&e) {
                    m.push(e);
                }
            }
            m
        });
        if self.modeldoc {
            for name in MODELDOC_METHODS {
                if !methods.iter().any(|m| m == name) {
                    methods.push(name.to_string());
                }
            }
        }
        // pet advertisement: `pet_loaded(true)` = all twelve from the start; `pet()` = only
        // `pet.load` until a fresh load adds the eleven `NEED_PET` ones (§0.3 state additions).
        if self.pet_loaded {
            for name in PET_METHODS {
                if !methods.iter().any(|m| m == name) {
                    methods.push(name.to_string());
                }
            }
        } else if self.pet && !methods.iter().any(|m| m == "pet.load") {
            methods.push("pet.load".to_string());
        }
        // renders (WP-D) advertisement: `.renders()` = both methods (the bridge's m2.5 rows, [V runtime]).
        if self.renders {
            for name in RENDER_METHODS {
                if !methods.iter().any(|m| m == name) {
                    methods.push(name.to_string());
                }
            }
        }
        if self.bridge_jobs {
            for name in ["job.start", "job.status", "job.cancel"] {
                if !methods.iter().any(|m| m == name) {
                    methods.push(name.to_string());
                }
            }
        }
        // hammer (WP-M + WP-R2 + WP-T) advertisement: the twenty built-in methods (like the
        // modeldoc/pet arms).
        // Pre-m2.18 modelling (`.hammer_value_undo(false)`, review w2b #5): the advertised set
        // drops the m2.18 marker `node.setTransform` with it — a real pre-m2.18 build does not
        // advertise it either.
        if self.hammer_nodes.is_some() {
            for name in HAMMER_METHODS {
                if name == "node.setTransform" && !self.hammer_value_undo_recorded {
                    continue;
                }
                if !methods.iter().any(|m| m == name) {
                    methods.push(name.to_string());
                }
            }
            if self.hammer_selection {
                for name in ["selection.get", "selection.set"] {
                    if !methods.iter().any(|m| m == name) {
                        methods.push(name.to_string());
                    }
                }
            }
            // m3.12: `mesh.get` is its own knob (the 20-name array above is the pre-m3.12 set).
            if self.hammer_mesh_get && !methods.iter().any(|m| m == "mesh.get") {
                methods.push("mesh.get".to_string());
            }
            // m3.14: `mesh.setFaceTexture` is its own knob too (default on).
            if self.hammer_face_texture && !methods.iter().any(|m| m == "mesh.setFaceTexture") {
                methods.push("mesh.setFaceTexture".to_string());
            }
        }
        let doc = match self.doc {
            Some(d) => d,
            None => fx.doc_result.get("doc").filter(|d| !d.is_null()).cloned(),
        };
        // the hammer world seeds doc path/dirty from the builder doc (default: the recorded
        // probe map, so save/undo tests are one `.hammer_nodes(vec![MockNode::world()])` away)
        let hammer = self.hammer_nodes.map(|nodes| {
            let doc_obj = doc.as_ref().and_then(Value::as_object);
            let path = doc_obj
                .and_then(|o| o.get("path"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let dirty = doc_obj
                .and_then(|o| o.get("dirty"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // the host always has the document's own file; `.hammer_map_files` adds the rest
            // (lower case: the gate compares case-insensitively, like the bridge)
            let mut map_files: Vec<String> = self
                .hammer_map_files
                .iter()
                .map(|f| f.to_ascii_lowercase())
                .collect();
            if let Some(rel) = hammer::addon_rel_of(&path) {
                if !map_files.contains(&rel) {
                    map_files.push(rel);
                }
            }
            let mut world = HammerWorld::new(
                nodes,
                path,
                dirty,
                self.hammer_editable,
                self.hammer_new_file,
                map_files,
            );
            world.set_drop_faces(self.hammer_drop_faces);
            world.set_drop_conn_readback(self.hammer_drop_conn_readback);
            world.set_value_undo_recorded(self.hammer_value_undo_recorded);
            if self.hammer_selection {
                world.enable_selection();
            }
            world.set_mesh_get(self.hammer_mesh_get);
            world.set_face_texture(self.hammer_face_texture);
            world.set_mesh_too_large(self.hammer_mesh_too_large);
            world
        });
        let state = Arc::new(Mutex::new(State {
            methods,
            responders: self.responders,
            errors: self.errors,
            nth_errors: self.nth_errors,
            calls: HashMap::new(),
            doc,
            docs: self.docs,
            model_doc: self.model_doc.unwrap_or_default(),
            node_types: self
                .node_types
                .unwrap_or_else(|| DEFAULT_NODE_TYPES.iter().map(|s| s.to_string()).collect()),
            pet_loaded: self.pet_loaded,
            hammer_not_loaded: false,
            pet_doc: self.pet_doc.unwrap_or_default(),
            pet_types: self.pet_types.unwrap_or_else(default_pet_types),
            pet_schemas: default_pet_schemas(self.pet_schemas_manual.clone()),
            pet_unloadable: self.pet_unloadable,
            pet_text: self.pet_text.unwrap_or_else(|| MOCK_PROBE_TEXT.to_string()),
            pet_undo: Vec::new(),
            pet_redo: Vec::new(),
            pet_render_error: self.pet_render_fail,
            pet_render_seq: 0,
            modeldoc_render_error: self.modeldoc_render_fail,
            modeldoc_render_seq: 0,
            modeldoc_no_session: self.modeldoc_no_session,
            view_render_fail: self.view_render_fail,
            world_no_map: self.world_no_map,
            world_no_scene_world: self.world_no_scene_world,
            render_dir: self.render_dir,
            view_render_seq: 0,
            world_render_seq: 0,
            hammer,
            main_thread_age_ms: self.main_thread_age_ms,
            drain_blocked: self.drain_blocked,
            queue_depth: self.queue_depth,
            shutting_down: self.shutting_down,
            inflight: self.inflight,
            revision: self.revision,
            delay_methods: self.delay_methods,
            silent_methods: self.silent_methods,
            drop_methods: self.drop_methods,
            jobs: Vec::new(),
            bridge_jobs: jobs::MockJobs {
                scripts: self.job_scripts.into(),
                unavailable: self.job_unavailable,
                writes: self.job_writes,
                content_root: self.job_content_root,
                ..Default::default()
            },
            requests: Vec::new(),
            authed: false,
            client_count: 0,
            busy_count: 0,
        }));
        let listener = TcpListener::bind((self.host.as_str(), 0)).await?;
        let addr = listener.local_addr()?;
        let dir = self.discovery_dir.unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "cs2mcp-mock-{}-{}",
                std::process::id(),
                DIR_SEQ.fetch_add(1, Ordering::Relaxed)
            ))
        });
        std::fs::create_dir_all(&dir)?;
        let discovery_path = dir.join("session.json");
        let started = Instant::now();
        let shared = Arc::new(Shared {
            fx,
            state: state.clone(),
            protocol: self.protocol,
            build: build.clone(),
            game_build: game_build.clone(),
            token: token.clone(),
            hello_timeout: self.hello_timeout,
            started,
            tasks: Mutex::new(Vec::new()),
        });
        let mb = MockBridge {
            host: addr.ip().to_string(),
            port: addr.port(),
            token,
            build,
            game_build,
            protocol: self.protocol,
            discovery_path,
            discovery_dir: dir,
            shared: shared.clone(),
            accept: Mutex::new(None),
        };
        mb.write_discovery()?;
        let accept_shared = shared.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let s = accept_shared.clone();
                let busy = {
                    let mut st = s.state.lock().unwrap();
                    st.client_count += 1;
                    if st.authed {
                        st.busy_count += 1;
                        true
                    } else {
                        false
                    }
                };
                if busy {
                    // the real bridge answers a second client on accept (no request required)
                    let env = s.envelope(Value::Null, None, Some(s.fx.busy.clone()));
                    let mut sock = sock;
                    let _ = sock.write_all(format!("{env}\n").as_bytes()).await;
                    let _ = sock.shutdown().await;
                    continue;
                }
                let worker = s.clone();
                let t = tokio::spawn(async move { worker.serve_client(sock).await });
                s.tasks.lock().unwrap().push(t);
            }
        });
        *mb.accept.lock().unwrap() = Some(handle);
        Ok(mb)
    }
}

struct Shared {
    fx: Fixtures,
    state: Arc<Mutex<State>>,
    protocol: u64,
    build: String,
    game_build: String,
    token: String,
    hello_timeout: Duration,
    started: Instant,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Shared {
    fn envelope(&self, id: Value, result: Option<Value>, error: Option<Value>) -> Value {
        let mut env = json!({"jsonrpc": "2.0", "id": id, "bridge": {"protocol": self.protocol, "build": self.build}});
        match error {
            Some(e) => env["error"] = e,
            None => env["result"] = result.unwrap_or(Value::Null),
        }
        env
    }

    fn hello_result(&self) -> Value {
        let mut res = self.fx.hello_result.clone();
        if !res.is_object() {
            res = json!({});
        }
        res["protocol"] = json!(self.protocol);
        res["bridgeVersion"] = json!(self.build);
        res["gameBuild"] = json!(self.game_build);
        if !res.get("capabilities").is_some_and(Value::is_object) {
            res["capabilities"] = json!({});
        }
        let methods = self.state.lock().unwrap().methods.clone();
        res["capabilities"]["methods"] = json!(methods);
        let caps = res["capabilities"].as_object_mut().unwrap();
        caps.entry("features").or_insert_with(|| json!({}));
        caps.entry("limits").or_insert_with(|| json!({}));
        res
    }

    async fn serve_client(self: Arc<Self>, sock: TcpStream) {
        let (rd, mut wr) = sock.into_split();
        let mut lines = BufReader::new(rd).lines();
        let mut authed = false;
        loop {
            let next = if authed {
                lines.next_line().await
            } else {
                match tokio::time::timeout(self.hello_timeout, lines.next_line()).await {
                    Ok(r) => r,
                    Err(_) => break,
                }
            };
            let Ok(Some(line)) = next else { break };
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => {
                    let _ = send(
                        &mut wr,
                        &self.envelope(
                            Value::Null,
                            None,
                            Some(json!({"code": -32700, "message": "ParseError"})),
                        ),
                    )
                    .await;
                    break;
                }
            };
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            if !authed {
                if req.get("method").and_then(Value::as_str) != Some("control.hello") {
                    let _ = send(
                        &mut wr,
                        &self.envelope(
                            id,
                            None,
                            Some(json!({"code": -32001, "message": "NotAuthenticated"})),
                        ),
                    )
                    .await;
                    break;
                }
                let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
                // like the real bridge (and the Python mock), hello is checked against the protocol
                // constant; a mock with another `protocol` still answers and the client must reject it
                if params.get("protocol").and_then(Value::as_u64)
                    != Some(crate::bridge::PROTOCOL as u64)
                {
                    let _ = send(
                        &mut wr,
                        &self.envelope(
                            id,
                            None,
                            Some(json!({"code": -32002, "message": "VersionMismatch"})),
                        ),
                    )
                    .await;
                    break;
                }
                if params.get("token").and_then(Value::as_str) != Some(self.token.as_str()) {
                    let _ = send(
                        &mut wr,
                        &self.envelope(
                            id,
                            None,
                            Some(json!({"code": -32001, "message": "NotAuthenticated"})),
                        ),
                    )
                    .await;
                    break;
                }
                authed = true;
                self.state.lock().unwrap().authed = true;
                let _ = send(&mut wr, &self.envelope(id, Some(self.hello_result()), None)).await;
                continue;
            }
            if !self.dispatch(&mut wr, &req).await {
                break;
            }
        }
        if authed {
            self.state.lock().unwrap().authed = false;
        }
        let _ = wr.shutdown().await;
    }

    /// Returns false when the connection must be closed.
    async fn dispatch(&self, wr: &mut tokio::net::tcp::OwnedWriteHalf, req: &Value) -> bool {
        let Some(method) = req
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            let _ = send(
                wr,
                &self.envelope(
                    req.get("id").cloned().unwrap_or(Value::Null),
                    None,
                    Some(json!({"code": -32600, "message": "invalid request: method required"})),
                ),
            )
            .await;
            return true;
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            let _ = send(
                wr,
                &self.envelope(Value::Null, None, Some(self.fx.invalid_request.clone())),
            )
            .await;
            return true;
        }
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        // snapshot the knobs under the lock, then act without holding it across awaits
        let (silent, drop_it, delay_ms, error, responder, advertised, builtin) = {
            let mut st = self.state.lock().unwrap();
            st.requests.push(RecordedRequest {
                method: method.clone(),
                params: params.clone(),
                id: id.clone(),
            });
            let nth = {
                let n = st.calls.entry(method.clone()).or_insert(0);
                *n += 1;
                *n
            };
            let nth_error = st
                .nth_errors
                .get(&method)
                .filter(|(k, _)| *k == nth)
                .map(|(_, e)| e.clone());
            let error = st.errors.get(&method).cloned().or(nth_error);
            // an error knob only skips the built-in for the bridge's **pre-write** refusals
            // (`-32602`/`-32004`/`-32022`/...): those never execute the op, and letting the world
            // mutate behind one made the reply and the world disagree (the review-2 M3 undo
            // serial is one of the things that disagree). A post-write `-32603` (the reply can
            // only come after the op applied, like a `mesh.setGeometry` that cleared the mesh
            // before it failed) is still modelled *with* the side effect — see
            // `a_protocol_error_on_a_write_is_reported_as_maybe_executed`, whose keyvalues really
            // change — so it runs, and the world and the tool's `outcomeUnknown` stay in step.
            let executed_behind_error = matches!(
                error,
                Some((-32603, _, _)) | Some((-32700, _, _)) | Some((-32600, _, _))
            );
            let builtin = if error.is_none() || executed_behind_error {
                self.builtin(&mut st, &method, &params)
            } else {
                None
            };
            (
                st.silent_methods.contains(&method),
                st.drop_methods.contains(&method),
                st.delay_methods.get(&method).copied().unwrap_or(0),
                error,
                st.responders.get(&method).cloned(),
                st.methods.contains(&method),
                builtin,
            )
        };
        if silent {
            return true;
        }
        if drop_it {
            return false;
        }
        if delay_ms > 0 {
            // the real bridge checks the deadline at dequeue: expired = QueueTimeout, never executed
            let deadline = params
                .get("deadlineMs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if deadline > 0 && delay_ms > deadline {
                let _ = send(
                    wr,
                    &self.envelope(
                        id,
                        None,
                        Some(json!({"code": -32008, "message": "QueueTimeout"})),
                    ),
                )
                .await;
                return true;
            }
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        if let Some((code, message, data)) = error {
            let mut e = json!({"code": code, "message": message});
            if let Some(d) = data {
                e["data"] = d;
            }
            let _ = send(wr, &self.envelope(id, None, Some(e))).await;
            return true;
        }
        if let Some(f) = responder {
            let _ = send(wr, &self.envelope(id, Some(f(&params)), None)).await;
            return true;
        }
        if let Some((result, error, close_after)) = builtin {
            let _ = send(wr, &self.envelope(id, result, error)).await;
            return !close_after;
        }
        if advertised {
            // advertised but no responder: a mock-only gap, not a bridge-level NotImplemented
            let _ = send(
                wr,
                &self.envelope(id, None, Some(json!({"code": -32011, "message": "NotImplemented", "data": {"mock": true, "method": method, "hint": "configure a responder or an error for this method in MockBridge"}}))),
            )
            .await;
            return true;
        }
        let _ = send(
            wr,
            &self.envelope(id, None, Some(self.fx.method_not_found.clone())),
        )
        .await;
        true
    }

    /// The built-in semantics. Mutable state: some built-ins mutate the mock's document state
    /// (`doc.active` is read-only, the ModelDoc ones update `dirty` and the path). Knobs
    /// (`silent`/`drop`/`delay`/`error`/`responder`) are applied before this, so they override
    /// every arm here (checked in `modeldoc_mock.rs`).
    #[allow(clippy::type_complexity)]
    fn builtin(
        &self,
        st: &mut State,
        method: &str,
        params: &Value,
    ) -> Option<(Option<Value>, Option<Value>, bool)> {
        // hammer (WP-M) entity built-ins take over doc.active and the entity/node/undo/save
        // semantics when the world is enabled; knobs (silent/drop/delay/error/responder) sit
        // in front of this like for every other built-in.
        if st.hammer_not_loaded && method == "doc.active" {
            // the real bridge (LIVE-P1b 01-session_status): the method exists but its identity
            // check has not run, because hammer.dll is not loaded
            return Some((
                None,
                Some(json!({"code": -32017, "message": "PrerequisiteFailed", "data": {
                    "check": "identity", "method": "doc.active", "module": "hammer.dll",
                    "state": "not_loaded"}})),
                false,
            ));
        }
        if st.hammer.is_some() {
            if let Some(r) = hammer::builtin(st, method, params) {
                return Some(r);
            }
        }
        let now = self.started.elapsed().as_millis() as u64;
        if matches!(method, "job.start" | "job.status" | "job.cancel") {
            // like the bridge, an unadvertised method answers MethodNotFound
            if !st.methods.iter().any(|m| m == method) {
                return None;
            }
            return jobs::builtin(&mut st.bridge_jobs, method, params);
        }
        match method {
            "control.ping" => {
                let mut res = json!({"ok": true, "uptimeMs": now, "drainBlocked": st.drain_blocked, "queueDepth": st.queue_depth, "inflight": st.inflight, "shuttingDown": st.shutting_down});
                if let Some(a) = st.main_thread_age_ms {
                    res["mainThreadAgeMs"] = json!(a);
                }
                if let Some(r) = st.revision {
                    res["revision"] = json!(r);
                }
                Some((Some(res), None, false))
            }
            "control.status" => {
                let mut listed = st.jobs.clone();
                listed.extend(st.bridge_jobs.summaries());
                let mut res = json!({"queue": [], "jobs": listed, "listener": true, "drainBlocked": st.drain_blocked, "shuttingDown": st.shutting_down});
                if let Some(r) = st.revision {
                    res["revision"] = json!(r);
                }
                Some((Some(res), None, false))
            }
            "control.capabilities" => {
                let mut res = self
                    .fx
                    .hello_result
                    .get("capabilities")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !res.is_object() {
                    res = json!({});
                }
                res["methods"] = json!(st.methods);
                if st.hammer_not_loaded {
                    // the real bridge reports every module's identity state (LIVE-P1b)
                    res["modules"] = json!({"hammer.dll": {"state": "not_loaded"}});
                }
                if let Some(r) = st.revision {
                    res["revision"] = json!(r);
                }
                Some((Some(res), None, false))
            }
            "control.version" => Some((
                Some(
                    json!({"protocol": self.protocol, "bridgeVersion": self.build, "gameBuild": self.game_build, "hostModules": []}),
                ),
                None,
                false,
            )),
            "control.cancel" => Some((
                None,
                Some(
                    json!({"code": -32010, "message": "NotCancellable", "data": {"reason": "not queued or already running"}}),
                ),
                false,
            )),
            "control.shutdown" => Some((Some(json!({"closing": true})), None, true)),
            "doc.active" => Some((
                Some(match &st.doc {
                    None => json!({"doc": null, "reason": "no_session"}),
                    Some(d) => json!({"doc": d}),
                }),
                None,
                false,
            )),
            "game.rules" => self.fx.game_rules.clone().map(|r| (Some(r), None, false)),
            "game.entities" => self
                .fx
                .game_entities
                .clone()
                .map(|r| (Some(r), None, false)),
            "doc.list" => {
                let docs: Vec<Value> = st
                    .docs
                    .clone()
                    .unwrap_or_else(|| st.doc.iter().cloned().collect());
                let active = docs
                    .iter()
                    .find(|d| d.get("active").and_then(Value::as_bool) == Some(true))
                    .and_then(|d| d.get("id").cloned());
                Some((Some(json!({"docs": docs, "activeId": active})), None, false))
            }
            // ---------------- ModelDoc (m2.11), semantics exactly the bridge method table -----------
            "modeldoc.active" => Some((
                Some(match &st.model_doc {
                    None => json!({"doc": null, "reason": "no_session"}),
                    Some(d) => json!({"doc": d}),
                }),
                None,
                false,
            )),
            "modeldoc.open" => {
                let path = params.get("path").and_then(Value::as_str).unwrap_or("");
                if path.is_empty() {
                    return Some((
                        None,
                        Some(json!({"code": -32602, "message": "InvalidParams", "data": {"reason": "path: a non-empty path relative to the addon content root is required"}})),
                        false,
                    ));
                }
                // OpenFile resolves relative to the addon content root (live: `models/...`)
                let win = format!(
                    "Z:\\app\\content\\csgo_addons\\{MOCK_ADDON}\\{}",
                    path.replace('/', "\\")
                );
                let doc = make_model_doc(&win, false);
                st.model_doc = Some(doc.clone());
                Some((
                    Some(json!({"opened": true, "path": path, "active": doc})),
                    None,
                    false,
                ))
            }
            "modeldoc.reload" => {
                let Some(d) = st.model_doc.as_mut() else {
                    return Some((None, Some(no_active_document()), false));
                };
                let dirty = d.get("dirty").and_then(Value::as_bool) == Some(true);
                let force = params.get("force").and_then(Value::as_bool) == Some(true);
                if dirty && !force {
                    return Some((
                        None,
                        Some(json!({"code": -32016, "message": "UnsavedChanges", "data": {"reason": "document is dirty; pass force:true to discard"}})),
                        false,
                    ));
                }
                let path = d
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                d["dirty"] = json!(false);
                Some((
                    Some(json!({"reloaded": true, "path": path, "discardedChanges": dirty})),
                    None,
                    false,
                ))
            }
            "modeldoc.save" => {
                let Some(d) = st.model_doc.as_mut() else {
                    return Some((None, Some(no_active_document()), false));
                };
                let new_file = d.get("isNewFile").and_then(Value::as_bool) == Some(true);
                let path = d
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if new_file || path.is_empty() {
                    return Some((
                        None,
                        Some(json!({"code": -32015, "message": "UntitledDocument"})),
                        false,
                    ));
                }
                let dirty = d.get("dirty").and_then(Value::as_bool) == Some(true);
                d["dirty"] = json!(false);
                Some((
                    Some(json!({"saved": true, "skippedClean": !dirty, "path": path})),
                    None,
                    false,
                ))
            }
            "modeldoc.compile" => {
                let Some(d) = st.model_doc.as_mut() else {
                    return Some((None, Some(no_active_document()), false));
                };
                if d.get("hasAsset").and_then(Value::as_bool) != Some(true) {
                    return Some((
                        None,
                        Some(json!({"code": -32017, "message": "PrerequisiteFailed", "data": {"reason": "no_asset"}})),
                        false,
                    ));
                }
                let dirty = d.get("dirty").and_then(Value::as_bool) == Some(true);
                d["dirty"] = json!(false);
                // the live bridge cannot read the compile status (a status-sink object, not a
                // string): `status`/`failed` are always null, the `.vmdl_c` mtime is the signal
                Some((
                    Some(json!({"status": null, "failed": null, "savedFirst": dirty})),
                    None,
                    false,
                ))
            }
            "modeldoc.nodeTypes" => Some((
                Some(json!({
                    "count": st.node_types.len(),
                    "types": st
                        .node_types
                        .iter()
                        .enumerate()
                        .map(|(i, c)| json!({"typeId": i, "className": c}))
                        .collect::<Vec<_>>(),
                })),
                None,
                false,
            )),
            // ---------------- pet (m2.12), semantics exactly the bridge method table §0.2 --------
            // Like the real bridge, an unadvertised `pet.*` method answers MethodNotFound (the
            // bits/loop bound would never route it); the arms below therefore only run when the
            // mock was built with `.pet()`/`.pet_loaded(..)` and the load state says so.
            "pet.load" | "pet.active" | "pet.open" | "pet.reload" | "pet.save"
            | "pet.functionTypes" | "pet.schema"
            | "pet.text" | "pet.apply" | "pet.undo" | "pet.redo" | "pet.render"
                if !st.methods.iter().any(|m| m == method) =>
            {
                None
            }
            // `pet.active`/`pet.functionTypes`/`pet.open`/... answer like the built-ins whether or
            // not `pet_loaded` is set (knobs and responders sit in front of this anyway); the
            // advertisement state is what tests of capability gating look at.
            "pet.load" => {
                let already = st.pet_loaded;
                st.pet_loaded = true;
                let mut advertised = false;
                for name in PET_POST_LOAD_METHODS {
                    if !st.methods.iter().any(|m| m == name) {
                        st.methods.push(name.to_string());
                        advertised = true;
                    }
                }
                if advertised {
                    // the client re-fetches capabilities after a state change
                    st.revision = Some(st.revision.map_or(1, |r| r + 1));
                }
                let tool_index = if already { None } else { Some(3) };
                Some((
                    Some(
                        json!({"loaded": true, "alreadyLoaded": already, "toolIndex": tool_index,
                               "module": "verified"}),
                    ),
                    None,
                    false,
                ))
            }
            "pet.active" => Some((
                Some(match &st.pet_doc {
                    None => json!({"doc": null, "reason": "no_session"}),
                    Some(d) => json!({"doc": d}),
                }),
                None,
                false,
            )),
            "pet.open" => {
                let path = params.get("path").and_then(Value::as_str).unwrap_or("");
                if path.is_empty() {
                    return Some((
                        None,
                        Some(json!({"code": -32602, "message": "InvalidParams", "data": {"reason": "path: a non-empty path relative to the addon content root is required"}})),
                        false,
                    ));
                }
                // OpenFile resolves relative to the addon content root (live: `particles/...`)
                let win = format!(
                    "Z:\\app\\content\\csgo_addons\\{MOCK_ADDON}\\{}",
                    path.replace('/', "\\")
                );
                // the bridge lex-checks the file before OpenFile touches it (test_pet step 11);
                // without the document existing, OpenFile cannot resolve it either
                if let Some(reason) = &st.pet_unloadable {
                    return Some((
                        None,
                        Some(unloadable_file(reason, &win)),
                        false,
                    ));
                }
                let doc = make_pet_doc(&win, false);
                st.pet_doc = Some(doc.clone());
                Some((
                    Some(json!({"opened": true, "path": path, "active": doc})),
                    None,
                    false,
                ))
            }
            "pet.reload" => {
                let Some(d) = st.pet_doc.as_mut() else {
                    return Some((None, Some(no_active_document()), false));
                };
                let path = d
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // the bridge lex-checks the file before ReloadDocument runs: it answers
                // UnloadableFile without the dirty check ever applying (nothing is modified)
                if let Some(reason) = &st.pet_unloadable {
                    return Some((None, Some(unloadable_file(reason, &path)), false));
                }
                let dirty = d.get("dirty").and_then(Value::as_bool) == Some(true);
                let force = params.get("force").and_then(Value::as_bool) == Some(true);
                if dirty && !force {
                    return Some((
                        None,
                        Some(json!({"code": -32016, "message": "UnsavedChanges", "data": {"reason": "document is dirty; pass force:true to discard"}})),
                        false,
                    ));
                }
                // `hasRoot` comes from the doc (tests set `hasRoot:false` to simulate the
                // failed-load state); `reloaded` = the session chain is still valid, which the
                // mock always is.
                let has_root = d.get("hasRoot").and_then(Value::as_bool) == Some(true);
                d["dirty"] = json!(false);
                Some((
                    Some(json!({"reloaded": true, "hasRoot": has_root, "path": path, "discardedChanges": dirty})),
                    None,
                    false,
                ))
            }
            "pet.save" => {
                let Some(d) = st.pet_doc.as_mut() else {
                    return Some((None, Some(no_active_document()), false));
                };
                let new_file = d.get("isNewFile").and_then(Value::as_bool) == Some(true);
                let path = d
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if new_file || path.is_empty() {
                    return Some((
                        None,
                        Some(json!({"code": -32015, "message": "UntitledDocument"})),
                        false,
                    ));
                }
                let dirty = d.get("dirty").and_then(Value::as_bool) == Some(true);
                // a clean document skips the API call entirely (nothing is written) unless the
                // caller forced the save, which re-serializes it in pet's own form
                let force = params.get("force").and_then(Value::as_bool) == Some(true);
                let skipped_clean = !dirty && !force;
                d["dirty"] = json!(false);
                Some((
                    Some(json!({"saved": true, "skippedClean": skipped_clean, "path": path})),
                    None,
                    false,
                ))
            }
            "pet.functionTypes" => Some((
                Some(json!({
                    "count": st.pet_types.len(),
                    "types": st.pet_types.clone(),
                })),
                None,
                false,
            )),
            "pet.schema" => {
                let Some(name) = params.get("class").and_then(Value::as_str) else {
                    return Some((
                        None,
                        Some(json!({"code": -32602, "message": "InvalidParams", "data": {"reason": "class: a class name is required"}})),
                        false,
                    ));
                };
                if name.is_empty() || name.len() > 127 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Some((
                        None,
                        Some(json!({"code": -32602, "message": "InvalidParams", "data": {"reason": "class: 1..127 chars, [A-Za-z0-9_] only"}})),
                        false,
                    ));
                }
                match st.pet_schemas.get(name) {
                    Some(s) => Some((Some(s.clone()), None, false)),
                    None => Some((
                        Some(json!({
                            "class": name, "found": false, "size": null, "project": null,
                            "bases": [], "fields": [], "metadata": []
                        })),
                        None,
                        false,
                    )),
                }
            }
            // --------- pet live edit + render (m2.13 plan2 §0.3) -----------------------------------
            // The mock does not parse KV3: `pet.apply`'s `arrays` in the reply and the document
            // are the knob's values, unchanged (`document.arrays` goes stale after an apply).
            // Undo/redo is a stack of full document texts (what `pet.text` serves next).
            "pet.text" => {
                if st.pet_doc.is_none() {
                    return Some((None, Some(no_active_document()), false));
                }
                let (path, dirty, is_new) = pet_doc_info(st.pet_doc.as_ref().unwrap());
                let text = st.pet_text.clone();
                Some((
                    Some(json!({
                        "text": text,
                        "bytes": text.len(),
                        "path": path,
                        "dirty": dirty,
                        "isNewFile": is_new,
                        "canUndo": !st.pet_undo.is_empty(),
                        "canRedo": !st.pet_redo.is_empty(),
                    })),
                    None,
                    false,
                ))
            }
            "pet.apply" => {
                let Some(text) = params.get("text").and_then(Value::as_str).filter(|t| !t.is_empty())
                else {
                    return Some((None, Some(invalid_params("text: a non-empty string is required")), false));
                };
                if text.len() > PET_TEXT_MAX {
                    return Some((
                        None,
                        Some(invalid_params("text: 1..393216 bytes")),
                        false,
                    ));
                }
                // the bridge's label gate: 1..64 printable ASCII, no quote or backslash
                if let Some(label) = params.get("label") {
                    let ok = label.as_str().is_some_and(|l| {
                        (1..=64).contains(&l.len())
                            && l.bytes().all(|b| (0x20..=0x7E).contains(&b) && b != b'"' && b != b'\\')
                    });
                    if !ok {
                        return Some((
                            None,
                            Some(invalid_params(
                                "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash",
                            )),
                            false,
                        ));
                    }
                }
                if st.pet_doc.is_none() {
                    return Some((None, Some(no_active_document()), false));
                }
                // the bridge lex-gates the text before anything is dispatched into pet
                if let Some(reason) = &st.pet_unloadable {
                    return Some((None, Some(unloadable_file(reason, "pet.apply:text")), false));
                }
                if !text.starts_with("<!-- kv3") {
                    return Some((
                        None,
                        Some(unloadable_file("no_kv3_header", "pet.apply:text")),
                        false,
                    ));
                }
                if !text.contains("_class = \"CParticleSystemDefinition\"") {
                    return Some((
                        None,
                        Some(unloadable_file("wrong_root_class", "pet.apply:text")),
                        false,
                    ));
                }
                let arrays = pet_doc_arrays(st.pet_doc.as_ref().unwrap());
                st.pet_undo.push(std::mem::take(&mut st.pet_text));
                st.pet_redo.clear();
                st.pet_text = text.to_string();
                if let Some(d) = st.pet_doc.as_mut() {
                    d["dirty"] = json!(true);
                }
                let label = params
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("MCP apply");
                Some((
                    Some(json!({
                        "applied": true,
                        "label": label,
                        "bytes": text.len(),
                        "dirty": true,
                        "canUndo": true,
                        "canRedo": false,
                        "arrays": arrays,
                    })),
                    None,
                    false,
                ))
            }
            "pet.undo" | "pet.redo" => {
                if st.pet_doc.is_none() {
                    return Some((None, Some(no_active_document()), false));
                }
                let is_undo = method == "pet.undo";
                let empty = if is_undo {
                    st.pet_undo.is_empty()
                } else {
                    st.pet_redo.is_empty()
                };
                let (_, dirty, _) = pet_doc_info(st.pet_doc.as_ref().unwrap());
                let arrays = pet_doc_arrays(st.pet_doc.as_ref().unwrap());
                let reason = if is_undo {
                    "nothing_to_undo"
                } else {
                    "nothing_to_redo"
                };
                if empty {
                    // a result, not an error: the bridge never crashes on an empty history
                    return Some((
                        Some(json!({
                            "done": false,
                            "reason": reason,
                            "dirty": dirty,
                            "canUndo": !st.pet_undo.is_empty(),
                            "canRedo": !st.pet_redo.is_empty(),
                            "arrays": arrays,
                        })),
                        None,
                        false,
                    ));
                }
                let cur = std::mem::take(&mut st.pet_text);
                if is_undo {
                    st.pet_redo.push(cur);
                    st.pet_text = st.pet_undo.pop().expect("undo stack checked non-empty");
                } else {
                    st.pet_undo.push(cur);
                    st.pet_text = st.pet_redo.pop().expect("redo stack checked non-empty");
                }
                if let Some(d) = st.pet_doc.as_mut() {
                    d["dirty"] = json!(true);
                }
                Some((
                    Some(json!({
                        "done": true,
                        "dirty": true,
                        "canUndo": !st.pet_undo.is_empty(),
                        "canRedo": !st.pet_redo.is_empty(),
                        "arrays": arrays,
                    })),
                    None,
                    false,
                ))
            }
            // The shared render semantics of `pet.render` and `modeldoc.render` (m2.13 / m2.14):
            // the same params, bounds, name rule and result shape; only the failure knobs and
            // the `hasDocument` source differ, so both arms fall through to
            // [`toolscene_render_reply`]. Like the pet gate above, an unadvertised
            // `modeldoc.render` gets no built-in (MethodNotFound from the fixture).
            "modeldoc.render" if !st.methods.iter().any(|m| m == method) => None,
            "pet.render" => toolscene_render_reply(
                params,
                None,
                st.pet_render_error.as_ref(),
                st.pet_doc.is_some(),
                &mut st.pet_render_seq,
            ),
            "modeldoc.render" => toolscene_render_reply(
                params,
                (st.modeldoc_no_session || st.model_doc.is_none())
                    .then(|| no_viewport("no_session")),
                st.modeldoc_render_error.as_ref(),
                st.model_doc.is_some(),
                &mut st.modeldoc_render_seq,
            ),
            // ---------------- Hammer renders (WP-D: view.render / world.render, m2.5) -----------
            // Like the pet gate above: an unadvertised render method answers MethodNotFound (the
            // fixture baseline advertises neither; `.renders()` advertises both).
            "view.render" | "world.render" if !st.methods.iter().any(|m| m == method) => None,
            "view.render" | "world.render" => hammer_render_reply(st, method, params),
            _ => None,
        }
    }
}

/// The one render built-in body `pet.render` and `modeldoc.render` share: parameters first
/// (nothing is validated after), then the chain/`RenderFailed` refusal, then the result object
/// with the mock's synthetic stats (the mock writes no file; tests write the PNG the bridge
/// would have written). Returns `(Some(result), None, false)`, `(None, error, false)` for a
/// refusal, or `None` when the method is not one of the two.
fn toolscene_render_reply(
    params: &Value,
    chain_error: Option<Value>,
    render_error: Option<&String>,
    has_document: bool,
    seq: &mut u32,
) -> Option<(Option<Value>, Option<Value>, bool)> {
    // parameters first (nothing is validated after), the same bounds and name rules
    // the `world.render`/`view.render` arms use
    let size = |key: &str, dflt: u64| -> Result<u64, String> {
        match params.get(key) {
            None => Ok(dflt),
            Some(v) => match v.as_u64() {
                Some(n) if (16..=4096).contains(&n) => Ok(n),
                _ => Err("width/height: 16..4096 each, at most 8388608 pixels".into()),
            },
        }
    };
    let width = match size("width", 1024) {
        Ok(w) => w,
        Err(reason) => return Some((None, Some(invalid_params(&reason)), false)),
    };
    let height = match size("height", 768) {
        Ok(h) => h,
        Err(reason) => return Some((None, Some(invalid_params(&reason)), false)),
    };
    if width * height > 8388608 {
        return Some((
            None,
            Some(invalid_params("width/height: 16..4096 each, at most 8388608 pixels")),
            false,
        ));
    }
    // render_name (mcp_bridge.c): [A-Za-z0-9._-], max 64, no leading dot; a trailing
    // ".png" (any case) is dropped; no usable name -> render_<seq> (the shared rule of the
    // Hammer render arms below, [`render_name_rule`]).
    let stem = match render_name_rule(params) {
        Err(reason) => return Some((None, Some(invalid_params(&reason)), false)),
        Ok(None) => {
            *seq += 1;
            format!("render_{}", *seq)
        }
        Ok(Some(base)) => base,
    };
    // after the params: the chain refusal (checked before any render object is touched), then
    // the `RenderFailed` knob, then the result object
    if let Some(e) = chain_error {
        return Some((None, Some(e), false));
    }
    if let Some(reason) = render_error {
        return Some((
            None,
            Some(json!({"code": -32020, "message": "RenderFailed", "data": {"reason": reason}})),
            false,
        ));
    }
    let file = format!("{stem}.png");
    let path = format!("Z:\\app\\renders\\{file}");
    let npx = width * height;
    Some((
        Some(json!({
            "path": path,
            "file": file,
            "width": width,
            "height": height,
            "bytes": 0,
            "format": "RGBA8888",
            "hasDocument": has_document,
            "stats": {
                "untouchedPixels": 0,
                "nonBlackPixels": npx,
                "meanLuma": 100,
                "minLuma": 0,
                "maxLuma": 255,
                "crc32": "00000000",
            },
            "durationMs": 1,
        })),
        None,
        false,
    ))
}

/// The bridge's `render_name` rule (mcp_bridge.c), now shared by the `pet.render` /
/// `modeldoc.render` arm and the Hammer render arms: a trailing ".png" (any case) is dropped,
/// then `[A-Za-z0-9._-]`, 1..64 chars, no leading dot. `Err(reason)` = the bridge's `-32602`
/// InvalidParams; `Ok(None)` = no usable name given (a non-string name, like the bridge's
/// `json_string` refusal, falls to the default stem).
fn render_name_rule(params: &Value) -> Result<Option<String>, String> {
    let Some(raw) = params.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let base = if raw.to_ascii_lowercase().ends_with(".png") && raw.len() > 4 {
        &raw[..raw.len() - 4]
    } else {
        raw
    };
    let ok = !base.is_empty()
        && base.len() <= 64
        && !base.starts_with('.')
        && base
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-');
    if !ok {
        return Err("name: [A-Za-z0-9._-], 1..64 chars, no leading dot".into());
    }
    Ok(Some(base.to_string()))
}

/// `-32021 NoGameWorld` the bridge answers when no map is loaded in the engine
/// (`op_world_render`, the exact reason string).
fn no_game_world() -> Value {
    json!({
        "code": -32021, "message": "NoGameWorld",
        "data": {"reason": "no map is loaded in the engine (map <name>)"},
    })
}

/// One camera tuple field the Hammer render arms echo back: three numbers from the request,
/// or the plausible viewport default when absent (the POC `support.render_responder` values).
fn render_vec3(params: &Value, key: &str, dflt: [f64; 3]) -> Value {
    match params.get(key).and_then(Value::as_array) {
        Some(a) if a.len() == 3 && a.iter().all(|v| v.as_f64().is_some()) => {
            json!(a.iter().map(Value::as_f64).collect::<Vec<_>>())
        }
        _ => json!(dflt),
    }
}

/// One camera scalar the render arms echo back (fov/znear/zfar): the request's number or the
/// default (view: 90 / 10 / 25000, world: 90 / 4 / 30000 — `op_view_render`/`op_world_render`).
fn render_num(params: &Value, key: &str, dflt: f64) -> Value {
    json!(params.get(key).and_then(Value::as_f64).unwrap_or(dflt))
}

/// `-32003 NoActiveDocument` the ModelDoc arms answer when the mock has no document (the mock
/// cannot know which of the bridge's reasons applies; `no_session` matches `modeldoc.active`).
fn no_active_document() -> Value {
    json!({"code": -32003, "message": "NoActiveDocument", "data": {"reason": "no_session"}})
}

/// The Hammer render built-ins (WP-D: `view.render` + `world.render`, semantics exactly
/// `mcp_bridge.c` `op_view_render` / `op_world_render` plus the dispatch gates). The
/// chain refusals come first (the C handlers gate on the object chain before any parameter,
/// and the `view.render` dispatch answers `-32003 NoActiveDocument` when there is no active
/// document), then the size/fov/name bounds, then the result object with the mock's synthetic
/// stats and camera. The mock renders nothing: without `render_dir` it reports `bytes: 0` (the
/// `pet.render`/`modeldoc.render` shape, where the tests write the file); with `render_dir` it
/// writes a tiny valid 8x4 PNG (solid colour, like the POC `tests/support.py::make_png`) under
/// the host directory `Z:\app\renders\<file>` maps to and reports its real length in `bytes`.
fn hammer_render_reply(
    st: &mut State,
    method: &str,
    params: &Value,
) -> Option<(Option<Value>, Option<Value>, bool)> {
    let is_view = method == "view.render";
    // -- the chain refusals (before any parameter is touched) --
    if is_view {
        if st.doc.is_none() {
            return Some((None, Some(no_active_document()), false));
        }
        if let Some(reason) = &st.view_render_fail {
            return Some((None, Some(no_viewport(reason)), false));
        }
    } else if st.world_no_scene_world {
        // the only -32021 case the live bridge answers (`GetGameSceneWorld` null): an engine
        // with no scene world object at all. review-2 L6: a *loaded-but-empty* engine (no map,
        // after `disconnect`) still renders its empty scene world — a black image, which is the
        // `world_no_map` knob below.
        return Some((None, Some(no_game_world()), false));
    }
    // -- world.render has no viewport camera to default to: origin + angles are required first,
    //    in the bridge's own order (op_world_render's parse errors, mcp_bridge.c 2558-2559 --
    //    review-2 N4: the mock used to run these checks last, so a call with several bad
    //    arguments got a different message than the real one) --
    if !is_view && params.get("origin").is_none() {
        return Some((None, Some(invalid_params("origin: [x,y,z] required")), false));
    }
    if !is_view && params.get("angles").is_none() {
        return Some((
            None,
            Some(invalid_params("angles: [pitch,yaw,roll] required")),
            false,
        ));
    }
    // -- sizes (op_view_render: 16..4096 & 8388608 px; op_world_render: 16..8192 & 33554432 px) --
    let (max_side, max_pixels): (u64, u64) = if is_view {
        (4096, 8_388_608)
    } else {
        (8192, 33_554_432)
    };
    let size = |key: &str, dflt: u64| -> Result<u64, Value> {
        match params.get(key) {
            None => Ok(dflt),
            Some(v) => match v.as_u64() {
                Some(n) if (16..=max_side).contains(&n) => Ok(n),
                _ => Err(invalid_params(&format!(
                    "width/height: 16..{max_side} each, at most {max_pixels} pixels"
                ))),
            },
        }
    };
    let (width, height) = if is_view {
        (size("width", 640), size("height", 360))
    } else {
        (size("width", 1920), size("height", 1080))
    };
    let (width, height) = match (width, height) {
        (Ok(w), Ok(h)) if w * h <= max_pixels => (w, h),
        _ => {
            return Some((
                None,
                Some(invalid_params(&format!(
                    "width/height: 16..{max_side} each, at most {max_pixels} pixels"
                ))),
                false,
            ))
        }
    };
    // -- fov (validated only when provided, like the bridge's range check) --
    let fov = match params.get("fov") {
        None => 90.0,
        Some(v) => match v.as_f64() {
            Some(f) if f > 1.0 && f < 179.0 => f,
            _ => {
                let reason = if is_view {
                    "fov: (1, 179) degrees"
                } else {
                    "fov: (1, 179) degrees, horizontal"
                };
                return Some((None, Some(invalid_params(reason)), false));
            }
        },
    };
    // -- znear / zfar (the bridge's gate: znear > 0, zfar in (znear, 1e6]); the defaults are the
    //    bridge's own fallbacks (op_view_render: 1/9000 -- the thumbnail values a viewport
    //    frustum without valid planes degrades to --, op_world_render: 4/30000; review-2 N4:
    //    the mock used to echo 10/25000, which is neither) --
    let (zn, zf) = if is_view { (1.0, 9000.0) } else { (4.0, 30000.0) };
    let znear = params.get("znear").and_then(Value::as_f64).unwrap_or(zn);
    let zfar = params.get("zfar").and_then(Value::as_f64).unwrap_or(zf);
    if !(znear > 0.0 && zfar > znear && zfar <= 1_000_000.0) {
        return Some((None, Some(invalid_params("znear/zfar")), false));
    }
    // -- the name (the shared `render_name` rule; default stem `render_<seq>`) --
    let stem = match render_name_rule(params) {
        Err(reason) => return Some((None, Some(invalid_params(&reason)), false)),
        Ok(None) => {
            let seq = if is_view {
                st.view_render_seq += 1;
                st.view_render_seq
            } else {
                st.world_render_seq += 1;
                st.world_render_seq
            };
            format!("render_{seq}")
        }
        Ok(Some(base)) => base,
    };

    // -- the result object (the C handlers' reply shape; the camera echoes the request) --
    let file = format!("{stem}.png");
    let path = format!("Z:\\app\\renders\\{file}");
    let npx = width * height;
    // -- the tiny valid PNG: with `render_dir` the arm writes it under the host directory
    //    (`Z:\app\renders\<file>` reaches `<dir>\<file>`) and reports its own length; without,
    //    `bytes: 0` and the tests write the file themselves. `world_no_map` renders the *empty*
    //    scene world (review-2 L6 live: no map loaded, after `disconnect`, is a uniform-black
    //    image plus the blank note, not an error): the black 8x4 PNG with all-zero stats.
    let (rgb, stats) = if !is_view && st.world_no_map {
        (
            [0u8, 0, 0],
            json!({
                "untouchedPixels": 0, "nonBlackPixels": 0, "meanLuma": 0.0,
                "minLuma": 0, "maxLuma": 0, "crc32": "00000000",
            }),
        )
    } else {
        let rgb: [u8; 3] = if is_view { [200, 120, 40] } else { [190, 150, 100] };
        (
            rgb,
            if is_view {
                json!({
                    "untouchedPixels": 0, "nonBlackPixels": npx, "meanLuma": 130.5,
                    "minLuma": 0, "maxLuma": 216, "crc32": "0badf00d",
                })
            } else {
                json!({
                    "untouchedPixels": 0, "nonBlackPixels": npx, "meanLuma": 175.5,
                    "minLuma": 0, "maxLuma": 255, "crc32": "cc45389d",
                })
            },
        )
    };
    let bytes = match &st.render_dir {
        Some(dir) => {
            let img = crate::capture::frame::RgbImage::new(
                8,
                4,
                rgb.iter().copied().cycle().take(96).collect(),
            );
            match img.and_then(|img| img.to_png()) {
                Ok(png) => {
                    std::fs::create_dir_all(dir).ok();
                    match std::fs::write(dir.join(&file), &png) {
                        Ok(()) => png.len() as u64,
                        Err(_) => 0,
                    }
                }
                Err(_) => 0,
            }
        }
        None => 0,
    };

    if !is_view {
        // review-2 N4: the real `op_world_render` reply has no `aspect` in its camera object
        // (only the view one does, mcp_bridge.c view reply), so the echo drops it
        return Some((
            Some(json!({
                "path": path, "file": file, "width": width, "height": height, "bytes": bytes,
                "camera": {
                    "origin": render_vec3(params, "origin", [0.0; 3]),
                    "angles": render_vec3(params, "angles", [0.0; 3]),
                    "fov": render_num(params, "fov", fov),
                    "znear": render_num(params, "znear", znear),
                    "zfar": render_num(params, "zfar", zfar),
                },
                "pipeline": "Tools->CSGO", "srgb": false,
                "stats": stats,
                "ms": {"render": 19, "readback": 180, "encode": 784},
            })),
            None,
            false,
        ));
    }
    let aspect = (width as f64 / height as f64 * 1000.0).round() / 1000.0;
    let has_origin = params.get("origin").is_some();
    let has_angles = params.get("angles").is_some();
    // "source" is the bridge's own report of what it used: explicit (origin+angles), mixed
    // (one of the two) or viewport (neither — the live 3D view's camera).
    Some((
        Some(json!({
            "path": path, "file": file, "width": width, "height": height, "bytes": bytes,
            "camera": {
                "origin": render_vec3(params, "origin", [-24.2, -581.5, 525.2]),
                "angles": render_vec3(params, "angles", [33.0, -6.0, 0.0]),
                "fov": render_num(params, "fov", fov),
                "aspect": aspect,
                "znear": render_num(params, "znear", znear),
                "zfar": render_num(params, "zfar", zfar),
                "source": match (has_origin, has_angles) {
                    (true, true) => "explicit",
                    (false, false) => "viewport",
                    _ => "mixed",
                },
            },
            "view": {"type": 3, "renderMode": 1, "views": 2},
            "toolMaterials": if params.get("hideToolMaterials").and_then(Value::as_bool) == Some(true) { "hidden" } else { "shown" },
            "format": 0, "srgb": true,
            "stats": {
                "untouchedPixels": 0, "nonBlackPixels": npx, "meanLuma": 130.5,
                "minLuma": 0, "maxLuma": 216, "crc32": "0badf00d",
            },
            "ms": {"render": 12, "encode": 3},
        })),
        None,
        false,
    ))
}

/// `-32019 NoViewport` the render methods answer upstream when the object chain is broken
/// (`toolscene_render_reply`'s mock version reports the reason, not the per-hop vtable).
fn no_viewport(reason: &str) -> Value {
    json!({"code": -32019, "message": "NoViewport", "data": {"reason": reason}})
}

/// The bridge's lex refusal for `pet.open`/`pet.reload`/`pet.apply` on text `kv3_lex.h` rejects
/// (open/reload path = the file's windows path; apply = `pet.apply:text`).
fn unloadable_file(reason: &str, path: &str) -> Value {
    json!({"code": -32602, "message": "UnloadableFile", "data": {"reason": reason, "path": path}})
}

/// `-32602 InvalidParams {reason}` (the built-ins' rejection shape, like `op_err`'s).
fn invalid_params(reason: &str) -> Value {
    json!({"code": -32602, "message": "InvalidParams", "data": {"reason": reason}})
}

/// (path, dirty, isNewFile) from a pet document object (missing keys read as empty/false).
fn pet_doc_info(doc: &Value) -> (String, bool, bool) {
    (
        doc.get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        doc.get("dirty").and_then(Value::as_bool) == Some(true),
        doc.get("isNewFile").and_then(Value::as_bool) == Some(true),
    )
}

/// The document's `arrays` (the knob's rows; the mock does not parse KV3) or an empty array.
fn pet_doc_arrays(doc: &Value) -> Value {
    doc.get("arrays").cloned().unwrap_or_else(|| json!([]))
}

async fn send(wr: &mut tokio::net::tcp::OwnedWriteHalf, obj: &Value) -> std::io::Result<()> {
    wr.write_all(format!("{obj}\n").as_bytes()).await
}

pub struct MockBridge {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub build: String,
    pub game_build: String,
    pub protocol: u64,
    pub discovery_path: PathBuf,
    discovery_dir: PathBuf,
    shared: Arc<Shared>,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl MockBridge {
    pub fn builder() -> MockBuilder {
        MockBuilder {
            fixtures: default_fixtures(),
            host: "127.0.0.1".into(),
            token: None,
            build: None,
            game_build: None,
            protocol: crate::bridge::PROTOCOL as u64,
            methods: None,
            responders: HashMap::new(),
            errors: HashMap::new(),
            nth_errors: HashMap::new(),
            doc: None,
            docs: None,
            model_doc: None,
            node_types: None,
            modeldoc: false,
            pet: false,
            pet_loaded: false,
            pet_doc: None,
            pet_types: None,
            pet_schemas_manual: HashMap::new(),
            pet_unloadable: None,
            pet_text: None,
            pet_render_fail: None,
            modeldoc_render_fail: None,
            modeldoc_no_session: false,
            renders: false,
            view_render_fail: None,
            world_no_map: false,
            world_no_scene_world: false,
            render_dir: None,
            hammer_nodes: None,
            hammer_new_file: false,
            hammer_editable: true,
            hammer_drop_faces: 0,
            hammer_drop_conn_readback: None,
            hammer_value_undo_recorded: true,
            hammer_selection: false,
            hammer_mesh_get: true,
            hammer_face_texture: true,
            hammer_mesh_too_large: false,
            hammer_map_files: Vec::new(),
            bridge_jobs: false,
            job_scripts: Vec::new(),
            job_unavailable: None,
            job_writes: Vec::new(),
            job_content_root: None,
            main_thread_age_ms: Some(5),
            drain_blocked: false,
            queue_depth: 0,
            shutting_down: false,
            inflight: 0,
            revision: None,
            delay_methods: HashMap::new(),
            silent_methods: HashSet::new(),
            drop_methods: HashSet::new(),
            hello_timeout: Duration::from_secs(5),
            discovery_dir: None,
        }
    }

    /// Default mock (fixture baseline, live main thread).
    pub async fn start() -> std::io::Result<MockBridge> {
        Self::builder().start().await
    }

    fn write_discovery(&self) -> std::io::Result<()> {
        let body = json!({
            "pid": std::process::id(), "protocol": self.protocol, "bridgeVersion": self.build,
            "gameBuild": self.game_build, "transport": "tcp", "host": self.host, "port": self.port,
            "pipeName": null, "token": self.token, "startedAt": 0,
        });
        let tmp = self.discovery_path.with_extension("tmp");
        std::fs::write(&tmp, format!("{body:#}\n"))?;
        std::fs::rename(&tmp, &self.discovery_path)
    }

    /// Overwrite the discovery file with arbitrary content (stale pid / bad token scenarios).
    pub fn write_discovery_raw(&self, body: &Value) -> std::io::Result<()> {
        std::fs::write(&self.discovery_path, format!("{body}\n"))
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.shared.state.lock().unwrap())
    }
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.with_state(|s| s.requests.clone())
    }
    pub fn method_calls(&self, method: &str) -> Vec<RecordedRequest> {
        self.with_state(|s| {
            s.requests
                .iter()
                .filter(|r| r.method == method)
                .cloned()
                .collect()
        })
    }
    pub fn reset_trace(&self) {
        self.with_state(|s| s.requests.clear())
    }
    pub fn busy_count(&self) -> u64 {
        self.with_state(|s| s.busy_count)
    }
    pub fn set_doc(&self, doc: Option<Value>) {
        self.with_state(|s| s.doc = doc)
    }
    /// Change the ModelDoc document `modeldoc.*` serves (`None` = no document).
    pub fn set_model_doc(&self, doc: Option<Value>) {
        self.with_state(|s| s.model_doc = doc)
    }
    /// Change the pet document `pet.*` serves (`None` = no document).
    pub fn set_pet_doc(&self, doc: Option<Value>) {
        self.with_state(|s| s.pet_doc = doc)
    }
    /// The hammer world's current node list (empty when the world is disabled; WP-M test seam).
    pub fn hammer_nodes(&self) -> Vec<MockNode> {
        self.with_state(|s| {
            s.hammer
                .as_ref()
                .map(|w| w.nodes.clone())
                .unwrap_or_default()
        })
    }

    /// m3.12 seam: Hammer's UI selected `ids` in `mode` (it becomes the current mode).
    pub fn hammer_ui_select_nodes(&self, mode: i32, ids: &[i64]) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.ui_select_nodes(mode, ids);
            }
        })
    }

    /// m3.12 seam: Hammer's UI selected these faces (`(mesh node id, indices)`).
    pub fn hammer_ui_select_faces(&self, picks: &[(i64, Vec<usize>)]) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.ui_select_faces(picks);
            }
        })
    }

    /// m3.12 knob: nodes node-selection refuses (locked/hidden) and meshes face-selection ignores.
    pub fn hammer_selection_filters(&self, nodes: &[i64], meshes: &[i64]) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.selection_filters(nodes, meshes);
            }
        })
    }

    /// m3.12: the selection model's `(mode, current-mode nodes, faces)`, `None` when off.
    pub fn hammer_selection_state(&self) -> Option<SelectionState> {
        self.with_state(|s| s.hammer.as_ref().and_then(|w| w.selection_state()))
    }

    /// m2.18: the live I/O connections the hammer world holds for `node_id` (empty when it has
    /// none — `entity.addConnections` writes them, `entity.removeConnections` removes them).
    pub fn hammer_connections(&self, node_id: i64) -> Vec<MockConnection> {
        self.with_state(|s| {
            s.hammer
                .as_ref()
                .and_then(|w| w.connections.get(&node_id).cloned())
                .unwrap_or_default()
        })
    }

    /// m2.18: `node.setTransform`'s stored scales for `node_id` (the identity `[1,1,1]` when the
    /// node never had them written, like a live node without object-properties scales).
    pub fn hammer_scales(&self, node_id: i64) -> [f64; 3] {
        self.with_state(|s| {
            s.hammer
                .as_ref()
                .and_then(|w| w.node_scales.get(&node_id).copied())
                .unwrap_or([1.0; 3])
        })
    }

    /// Store object-properties scales for `node_id` directly (the state a `node.setTransform` or a
    /// scaled `entity.create` leaves behind, without going through either). Used by tests that need
    /// the live `entity.get` to report non-identity scales — review W2 3c (the m2.18 read reports
    /// the node's C++ `scales` field).
    pub fn set_hammer_scales(&self, node_id: i64, scales: [f64; 3]) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.node_scales.insert(node_id, scales);
            }
        })
    }
    /// Change the document text `pet.text` serves and `pet.apply` overwrites; the undo/redo
    /// stacks are left alone (call this before building a fresh edit state, or clear the stacks
    /// through `with_state`).
    pub fn set_pet_text(&self, text: impl Into<String>) {
        self.with_state(|s| s.pet_text = text.into())
    }
    /// Flip the mock's pet load state. Going to `true` advertises the eleven `NEED_PET` methods
    /// (the mock was built with `.pet()`/`.pet_loaded(..)`); going back to `false` withdraws
    /// them. Bumps the revision either way so clients re-fetch capabilities.
    pub fn set_pet_loaded(&self, loaded: bool) {
        self.with_state(|s| {
            if s.pet_loaded == loaded {
                return;
            }
            s.pet_loaded = loaded;
            if loaded {
                for name in PET_POST_LOAD_METHODS {
                    if !s.methods.iter().any(|m| m == name) {
                        s.methods.push(name.to_string());
                    }
                }
            } else {
                s.methods.retain(|m| !PET_POST_LOAD_METHODS.contains(&m.as_str()));
            }
            s.revision = Some(s.revision.map_or(1, |r| r + 1));
        })
    }
    /// Model a fresh tools session before any document opened: hammer.dll is not loaded, so the
    /// hammer methods (`map.open`, `doc.active`, ...) are not advertised. `hammer_open_asset`
    /// loads it. Call before the first client connects (the hello carries the method list).
    pub fn hammer_unloaded(&self) {
        self.with_state(|s| {
            s.methods.retain(|m| !HAMMER_METHODS.contains(&m.as_str()) && m != "mesh.get");
            s.hammer_not_loaded = true;
            s.revision = Some(s.revision.map_or(1, |r| r + 1));
        })
    }
    /// The engine handled a VToolMessage `open_asset` of `path` (addon-relative `maps/x.vmap`):
    /// hammer.dll loads (the hammer methods are advertised again) and the map becomes the
    /// document, like the real editor's `OpenFile`. `Err` is the open's refusal (no such file).
    pub fn hammer_open_asset(&self, path: &str) -> Result<Value, Value> {
        self.with_state(|s| {
            let opened = match s.hammer.as_mut() {
                Some(world) => hammer::simulate_open_asset(world, path),
                None => Err(json!("the mock has no hammer world")),
            }?;
            s.hammer_not_loaded = false;
            for name in HAMMER_METHODS {
                if !s.methods.iter().any(|m| m == name) {
                    s.methods.push(name.to_string());
                }
            }
            s.revision = Some(s.revision.map_or(1, |r| r + 1));
            Ok(opened)
        })
    }
    /// Script the next job's outcome (consumed by successive `job.start`s, in order); `polls` is
    /// how many `job.status` answers say `running` before the terminal state.
    pub fn script_job(&self, outcome: jobs::MockJobOutcome, polls: u32) {
        self.with_state(|s| s.bridge_jobs.scripts.push_back(jobs::MockJobScript { outcome, polls }))
    }
    /// `job.start` answers `-32017 "job: another job is starting"` while set.
    pub fn set_job_starting(&self, starting: bool) {
        self.with_state(|s| s.bridge_jobs.starting = starting)
    }
    /// Every `job.start` answers `-32017 {blockedOn}` while `Some`.
    pub fn set_job_unavailable(&self, blocked_on: Option<&str>) {
        self.with_state(|s| s.bridge_jobs.unavailable = blocked_on.map(str::to_string))
    }
    /// Put a job into the mock's table as another caller's (another server sharing the bridge):
    /// `state` as given, `client_ref` as that caller's token (or none). Returns its id.
    pub fn add_foreign_job(&self, input: &str, client_ref: Option<&str>, state: &str) -> String {
        self.with_state(|s| {
            s.bridge_jobs.seq += 1;
            let id = format!("j{}-foreign{}", s.bridge_jobs.seq, s.bridge_jobs.seq);
            s.bridge_jobs.jobs.push(jobs::MockJob {
                id: id.clone(),
                input: input.to_string(),
                client_ref: client_ref.map(str::to_string),
                game: false,
                flags: Vec::new(),
                state: state.to_string(),
                script: jobs::MockJobScript { outcome: jobs::MockJobOutcome::Hang, polls: 0 },
                polls_seen: 0,
                started_ms: jobs::unix_ms(),
            });
            id
        })
    }
    /// The mock's job table (ids, inputs, flags, state).
    pub fn bridge_jobs(&self) -> Vec<jobs::MockJob> {
        self.with_state(|s| s.bridge_jobs.jobs.clone())
    }
    pub fn set_jobs(&self, jobs: Vec<Value>) {
        self.with_state(|s| s.jobs = jobs)
    }
    pub fn set_methods<I: IntoIterator<Item = T>, T: Into<String>>(&self, m: I) {
        self.with_state(|s| s.methods = m.into_iter().map(Into::into).collect())
    }
    pub fn set_revision(&self, r: Option<i64>) {
        self.with_state(|s| s.revision = r)
    }
    pub fn set_main_thread_age_ms(&self, v: Option<u64>) {
        self.with_state(|s| s.main_thread_age_ms = v)
    }
    pub fn set_drain_blocked(&self, v: bool) {
        self.with_state(|s| s.drain_blocked = v)
    }
    pub fn set_inflight(&self, v: i64) {
        self.with_state(|s| s.inflight = v)
    }
    pub fn clear_delays(&self) {
        self.with_state(|s| s.delay_methods.clear())
    }
    pub fn add_delay(&self, method: &str, ms: u64) {
        self.with_state(|s| {
            s.delay_methods.insert(method.into(), ms);
        })
    }
    pub fn add_silent(&self, method: &str) {
        self.with_state(|s| {
            s.silent_methods.insert(method.into());
        })
    }
    pub fn clear_silent(&self) {
        self.with_state(|s| s.silent_methods.clear())
    }
    pub fn add_drop(&self, method: &str) {
        self.with_state(|s| {
            s.drop_methods.insert(method.into());
        })
    }
    pub fn add_error(&self, method: &str, code: i64, message: &str, data: Option<Value>) {
        self.with_state(|s| {
            s.errors.insert(method.into(), (code, message.into(), data));
        })
    }
    pub fn add_responder_value(&self, method: &str, v: Value) {
        self.with_state(|s| {
            s.responders
                .insert(method.into(), Arc::new(move |_| v.clone()));
        })
    }

    /// The M1 knob at runtime: faces Hammer "welds away" from every geometry request.
    pub fn set_hammer_drop_faces(&self, drop: usize) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_drop_faces(drop);
            }
        })
    }

    /// m3.12: the transform the mock's `mesh.get` reports for `node_id` (see
    /// [`HammerWorld::set_mesh_transform`]).
    pub fn set_hammer_mesh_transform(&self, node_id: i64, xf: MeshTransform) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_mesh_transform(node_id, xf);
            }
        })
    }

    /// m2.17: `false` models a bridge **older than m2.17** — an undoable write's reply carries no
    /// `doc` tail, so hammer-mcp falls back to its own post-write `doc.active` read.
    pub fn set_hammer_write_doc(&self, v: bool) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_write_doc(v);
            }
        })
    }

    /// m2.18: `true` models the `entity.setKeyValues` change (`RecordNodeTree`): the reply says
    /// `valueUndo:"recorded"` and the popped `"MCP: set keyvalues"` group restores the values.
    /// The builder knob `MockBuilder::hammer_value_undo` is the whole-package setting (it also
    /// drops `node.setTransform` from the advertisement); this post-start setter only flips the
    /// reply-era behaviour of a world already built.
    pub fn set_hammer_value_undo_recorded(&self, v: bool) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_value_undo_recorded(v);
            }
        })
    }

    /// review-2 M3/L1 test seam: one Hammer UI edit made in the editor, without any MCP tool
    /// (its own undo group: `topSerial`/`undoDepth` move, the document goes dirty; the epoch
    /// stays, because a Hammer edit does not move the bridge's fingerprint).
    pub fn hammer_user_edit(&self, label: &str) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.user_edit(label);
            }
        })
    }

    /// review-3 NEW-1 test seam: the user presses Ctrl+Z in Hammer — the top group is popped
    /// (`undoDepth` and `topSerial` both go down, because `topSerial` is the *top* group's serial)
    /// and the undo **uses up one serial** (live, `serial-probe.log`), so the counter behind the
    /// reported one advances without anything being visible. Returns whether a group was popped; an
    /// empty stack is not an error. See [`HammerWorld::user_undo`] for what this models.
    pub fn hammer_user_undo(&self) -> bool {
        self.with_state(|s| s.hammer.as_mut().is_some_and(|world| world.user_undo()))
    }

    /// review-3 NEW-1 test seam: the user redoes what [`MockBridge::hammer_user_undo`] popped. The
    /// state comes back, but as a *new* group at the next serial (what Hammer's own redo does is
    /// unverified — the bridge has no `undo.redo` — and this is the conservative reading), so the
    /// sticky mesh-cache rule sees a difference and drops its entries (review-4 R4-1/R4-3). Returns
    /// whether a group came back.
    pub fn hammer_user_redo(&self) -> bool {
        self.with_state(|s| s.hammer.as_mut().is_some_and(|world| world.user_redo()))
    }

    /// review-3 NEW-1 assertions: how many Hammer-UI undos/redos the seams made.
    pub fn hammer_user_undos(&self) -> u64 {
        self.with_state(|s| s.hammer.as_ref().map(|w| w.user_undos()).unwrap_or(0))
    }

    pub fn hammer_user_redos(&self) -> u64 {
        self.with_state(|s| s.hammer.as_ref().map(|w| w.user_redos()).unwrap_or(0))
    }

    /// review-3 NEW-3 test seam: one concurrent Hammer-UI group right after the `n`th successful
    /// `mesh.create`/`mesh.setGeometry` reply (`None` = off).
    pub fn set_hammer_group_after_write(&self, n: Option<u64>) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_group_after_write(n);
            }
        })
    }

    /// review-2 W2 #3 test seam: the read-back of the `at`-th (0-based) `entity.addConnections`
    /// wire entry fails, so the reply carries `{"readback":false,...}` for it — the bridge's
    /// "handed to AddConnection but not confirmed, maybe-executed" case. `None` is off.
    pub fn set_hammer_drop_conn_readback(&self, at: Option<i64>) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_drop_conn_readback(at);
            }
        })
    }

    /// m3.14 test seam: make the next `mesh.setFaceTexture` calls fail as the bridge can.
    /// `"pre"`: `-32603 TextureSetUnknown`, nothing written. `"panic"`: the write lands, then
    /// `-32603 InternalError {reason:"exec panicked"}` (no `maybeExecuted`: the queue's panic
    /// reply). `"mismatch"`: the write lands, then `-32603 TextureWriteMismatch` with
    /// `maybeExecuted:true`. `None` is off.
    pub fn set_hammer_face_texture_fail(&self, mode: Option<&str>) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_face_texture_fail(mode.map(str::to_string));
            }
        })
    }

    /// review-2 L2 test seam: switch the document identity right after `reads` upcoming
    /// `doc.active` replies (`docSeq`/`epoch` move, like opening a second session live).
    pub fn set_hammer_doc_switch_after(&self, reads: Option<u64>) {
        self.with_state(|s| {
            if let Some(world) = s.hammer.as_mut() {
                world.set_doc_switch_after(reads);
            }
        })
    }

    /// The recorded M2.x capability list (what hello advertises by default).
    pub fn baseline_methods(&self) -> Vec<String> {
        self.shared
            .fx
            .hello_result
            .pointer("/capabilities/methods")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn stop(&self) {
        if let Some(h) = self.accept.lock().unwrap().take() {
            h.abort();
        }
        for t in self.shared.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.discovery_path);
        let _ = std::fs::remove_dir(&self.discovery_dir);
    }
}

impl Drop for MockBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A map document shaped like the recorded `doc.active` result (POC `support.make_doc`).
pub fn make_doc(path: &str, dirty: Option<bool>) -> Value {
    let mut doc = json!({
        "id": "d1", "path": path, "dirty": false, "active": true, "editable": true,
        "notEditableReason": "ok", "epoch": 1, "docType": "vmap", "isNewFile": false,
        "undoName": "MCP: batch", "undoDepth": 2, "topSerial": 4,
    });
    doc["dirty"] = json!(dirty);
    doc
}

/// A ModelDoc document shaped like the m2.11 `modeldoc.active` doc object (the real one comes
/// from `active_modeldoc` on the bridge; modeldoc-plan §2 WP4).
pub fn make_model_doc(path: &str, dirty: bool) -> Value {
    json!({
        "path": path, "dirty": dirty, "isNewFile": false, "hasAsset": true,
        "compileStatus": null, "docType": "vmdl",
    })
}

/// A particle document shaped like the m2.12 `pet.active` doc object (§0.2): all nine arrays
/// present, counts/classes matching the live-verified `probe.vpcf` (one emitter, one
/// initializer, two operators, one renderer; no children, no named-value locals).
pub fn make_pet_doc(path: &str, dirty: bool) -> Value {
    json!({
        "path": path, "dirty": dirty, "isNewFile": false, "hasRoot": true,
        "docType": "vpcf",
        "arrays": [
            {"type": 0, "count": 0, "classes": []},
            {"type": 1, "count": 1, "classes": ["C_OP_ContinuousEmitter"]},
            {"type": 2, "count": 1, "classes": ["C_INIT_CreateWithinSphereTransform"]},
            {"type": 3, "count": 2, "classes": ["C_OP_BasicMovement", "C_OP_Decay"]},
            {"type": 4, "count": 0, "classes": []},
            {"type": 5, "count": 0, "classes": []},
            {"type": 6, "count": 1, "classes": ["C_OP_RenderSprites"]},
            {"type": 7, "count": 0, "classes": []},
            {"type": 8, "count": 0, "classes": []},
        ],
    })
}

/// Default `pet.functionTypes` rows (0.2 `types[]` shape): the probe's four classes plus one
/// obsolete operator, indexes per type in manager order.
pub fn default_pet_types() -> Vec<Value> {
    vec![
        json!({
            "type": 1, "index": 0, "name": "Emit continuously",
            "className": "C_OP_ContinuousEmitter", "metadata": ["MGetKV3ClassDefaults"],
            "obsolete": false, "singleton": null,
        }),
        json!({
            "type": 2, "index": 0, "name": "Create within sphere transform",
            "className": "C_INIT_CreateWithinSphereTransform",
            "metadata": ["MGetKV3ClassDefaults"], "obsolete": false, "singleton": null,
        }),
        json!({
            "type": 3, "index": 0, "name": "Basic movement",
            "className": "C_OP_BasicMovement", "metadata": ["MGetKV3ClassDefaults"],
            "obsolete": false, "singleton": null,
        }),
        json!({
            "type": 3, "index": 1, "name": "Decay",
            "className": "C_OP_Decay", "metadata": ["MGetKV3ClassDefaults"],
            "obsolete": false, "singleton": null,
        }),
        json!({
            "type": 3, "index": 2, "name": "Old style (obsolete)",
            "className": "C_OP_OldStyleObsolete",
            "metadata": ["MObsoleteParticleFunction"], "obsolete": true, "singleton": null,
        }),
        json!({
            "type": 6, "index": 0, "name": "Render sprites",
            "className": "C_OP_RenderSprites", "metadata": ["MGetKV3ClassDefaults"],
            "obsolete": false, "singleton": null,
        }),
    ]
}

/// One 0.2 `pet.schema` reply (`found:true`); `fields` are already flattened like the snapshot
/// rules demand (base fields included with `declaredIn`).
fn pet_schema_reply(
    class: &str,
    size: i64,
    bases: &[&str],
    fields: &[(&str, &str, &str)],
    metadata: &[&str],
) -> Value {
    json!({
        "class": class,
        "found": true,
        "size": size,
        "project": null,
        "bases": bases,
        "fields": fields
            .iter()
            .map(|(name, ty, declared_in)| json!({
                "name": name, "type": ty, "declaredIn": declared_in,
            }))
            .collect::<Vec<_>>(),
        "metadata": metadata,
    })
}

/// Default `pet.schema` replies: the six default function classes plus the root class and the
/// two structural classes (§0.2 state additions). `extra` overrides/extends per class name.
fn default_pet_schemas(extra: HashMap<String, Value>) -> HashMap<String, Value> {
    let mut m: HashMap<String, Value> = HashMap::new();
    let insert = |m: &mut HashMap<String, Value>, v: Value| {
        m.insert(v["class"].as_str().unwrap().to_string(), v);
    };
    insert(
        &mut m,
        pet_schema_reply(
            "C_OP_ContinuousEmitter",
            144,
            &["CParticleFunctionEmitter", "CParticleFunction"],
            &[
                ("m_flEmitRate", "CParticleCollectionFloatInput", "C_OP_ContinuousEmitter"),
                ("m_nEmitControlPointNumber", "int32", "C_OP_ContinuousEmitter"),
                ("m_flOpStartFadeInTime", "CParticleCollectionFloatInput", "CParticleFunctionOperator"),
                ("m_flOpEndFadeInTime", "CParticleCollectionFloatInput", "CParticleFunctionOperator"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "C_INIT_CreateWithinSphereTransform",
            160,
            &["CParticleFunctionInitializer"],
            &[
                ("m_fRadiusMin", "CPerParticleFloatInput", "C_INIT_CreateWithinSphereTransform"),
                ("m_fRadiusMax", "CPerParticleFloatInput", "C_INIT_CreateWithinSphereTransform"),
                ("m_nControlPointNumber", "int32", "C_INIT_CreateWithinSphereTransform"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "C_OP_BasicMovement",
            200,
            &["CParticleFunctionOperator"],
            &[
                ("m_Gravity", "CParticleCollectionVecInput", "C_OP_BasicMovement"),
                ("m_fDrag", "CParticleCollectionFloatInput", "C_OP_BasicMovement"),
                ("m_flOpStartFadeInTime", "CParticleCollectionFloatInput", "CParticleFunctionOperator"),
                ("m_flOpEndFadeInTime", "CParticleCollectionFloatInput", "CParticleFunctionOperator"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "C_OP_Decay",
            16,
            &["CParticleFunctionOperator"],
            &[("m_flDecayTime", "CPerParticleFloatInput", "C_OP_Decay")],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "C_OP_OldStyleObsolete",
            16,
            &["CParticleFunctionOperator"],
            &[("m_flObsRate", "CPerParticleFloatInput", "C_OP_OldStyleObsolete")],
            &["MObsoleteParticleFunction"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "C_OP_RenderSprites",
            176,
            &["CParticleFunctionRenderer"],
            &[
                ("m_nRenderType", "int32", "C_OP_RenderSprites"),
                ("m_hTexture", "CStrongHandle<InfoForResourceTypeIMaterial2>", "C_OP_RenderSprites"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "CParticleSystemDefinition",
            1024,
            &[],
            &[("m_nMaxParticles", "uint32", "CParticleSystemDefinition")],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "ParticleChildrenInfo_t",
            24,
            &[],
            &[
                ("m_ChildRef", "CResourceNameTyped<resource::CParticleSystem>", "ParticleChildrenInfo_t"),
                ("m_flDelay", "float32", "ParticleChildrenInfo_t"),
            ],
            &[],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "ParticleNamedValueSource_t",
            32,
            &[],
            &[
                ("m_Name", "CUtlString", "ParticleNamedValueSource_t"),
                ("m_IsPublic", "bool", "ParticleNamedValueSource_t"),
                ("m_ValueType", "CPulseValueFullType", "ParticleNamedValueSource_t"),
                ("m_DefaultConfig", "ParticleNamedValueConfiguration_t", "ParticleNamedValueSource_t"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    insert(
        &mut m,
        pet_schema_reply(
            "ParticleNamedValueConfiguration_t",
            120,
            &[],
            &[
                ("m_ConfigName", "CUtlString", "ParticleNamedValueConfiguration_t"),
                ("m_ConfigValue", "KeyValues3", "ParticleNamedValueConfiguration_t"),
                ("m_BoundValuePath", "CUtlString", "ParticleNamedValueConfiguration_t"),
                ("m_iAttachType", "uint8", "ParticleNamedValueConfiguration_t"),
                ("m_strEntityScope", "CUtlString", "ParticleNamedValueConfiguration_t"),
                ("m_strAttachmentName", "CUtlString", "ParticleNamedValueConfiguration_t"),
            ],
            &["MGetKV3ClassDefaults"],
        ),
    );
    for (k, v) in extra {
        m.insert(k, v);
    }
    m
}

/// Convenience for tests: a params object.
pub fn obj(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<Map<_, _>>(),
    )
}
