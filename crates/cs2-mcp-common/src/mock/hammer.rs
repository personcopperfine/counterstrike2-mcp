//! The mock's Hammer entity world (WP-M, plan §3 + §4.1; semantics `bridge/mcp_bridge.c`
//! `op_entity_create` / `apply_keyvalues` / `op_set_keyvalues` / `edit.batch` / `node.*` /
//! `undo.undo` / `map.save` / `map.reload` / `doc.active`), plus the geometry world WP-B added
//! (`op_mesh_create` / `op_mesh_set_geometry` / `op_mesh_set_material` and their `geo_dispatch`
//! gates), the m2.18 prop writes (`op_set_transform`, `op_add_connections`,
//! `op_remove_connections`, the `connections` array of `entity.get`) and the m2.19 brush-entity
//! pair WP-T added (`op_tie_meshes` / `op_untie`, the `children` array of `entity.get`, the
//! `MockNode::parent` edge). The pair follows `target/plan-triggers.md` §5 (the FINAL wire
//! contract: `class? | entityID?`, `allowRetie`, `alreadyTied`/`removedEmptyEntities`, the
//! `"MCP: tie meshes to entity"` / `"Untie from entity"` labels, a new entity's origin = the
//! FIRST mesh's origin).
//!
//! Enabled with [`super::MockBuilder::hammer_nodes`]; that also advertises the twenty methods of
//! [`HAMMER_METHODS`] in hello. The state is a flat node list (the bridge's walk only visits
//! the world's children anyway) with a per-node `parent` edge for the m2.19 tie, an undo stack of
//! `(label, Option<snapshot>)` entries and a document fingerprint (`id "dN"` + `epoch`, bumped by
//! every mutation). Where the m2.14 bridge code and plan §3 (the m2.15 target) differ, plan §3
//! wins per the brief.
//!
//! Deliberate mirrors of the real bridge:
//! * every mutator answers `-32004 DocumentChanged {reason: missing|missing_id|
//!   missing_epoch|doc_id|epoch}` when `expectDoc` misses the fingerprint;
//! * handles issued by `entity.create` are only resolvable inside the same `edit.batch`
//!   request (`$<createIndex>`); any other handle -> `-32005 StaleHandle unknown_handle`;
//! * `edit.batch` opens one undo group — `"MCP: batch"` when the ops list contains an
//!   `entity.create` op, `"MCP: set keyvalues"` otherwise (the bridge's `batch_has_create`,
//!   decided on the listed ops before the group opens, 35528e8 / protocol-diff-m2.15.md item 16) —
//!   stops at the first failing op and reports `-32012 PartialFailure {results, failedIndex}` —
//!   no rollback;
//! * undoing a create/delete restores the node snapshot, undoing a keyvalue group
//!   (`MCP: set keyvalues`, including a batch without a create op) pops the entry **without**
//!   restoring values (the F2 gap, on purpose);
//! * `map.save` and `map.reload` on an untitled/new document -> `-32015 UntitledDocument`
//!   (FIX-3R M1 for the client-side pre-check, FIX-3B M1 for the bridge arm: both halves now carry
//!   the gate, so the mock is no longer "ahead of the bridge");
//! * `map.reload` (like the bridge: editable, `expectDoc`, then untitled) refuses a dirty document
//!   without `force` (`-32016 UnsavedChanges`); with `force` it reloads from disk (dirty flag
//!   cleared, the in-editor undo history dropped, the fingerprint epoch bumped, `discardedChanges` =
//!   the dirty flag seen before the reload);
//! * `map.open` mirrors the bridge's gates (m2.16): the path shape (review M1: the canonical
//!   addon-relative spelling only), the host file (the mock keeps the addon-relative paths it has,
//!   `-32014 AssetNotFound` otherwise) and the session-leak gate — a file one of the open sessions
//!   already holds answers `{opened:false, existing:true, active:<that session is the active
//!   one>}` and the document is NOT touched (the bridge does not call `OpenFile`: its session match
//!   is a stub, so a second call would create a second session for the same file; live the app keeps
//!   every session, review M2); any other file the host has becomes the document (new `id`, bumped
//!   `epoch`, clean, no undo history) and gains a session. The bridge's DMX header gate (H1(b)) is
//!   not mirrored: the Rust tool parses the file with its own reader before it dispatches (H1(a));
//! * an **empty** string keyvalue is applied like any other text (m2.16: Hammer stores the empty
//!   string as a NULL `CUtlSymbolLarge`, the bridge writes it and answers `applied:true`);
//! * a value above the bridge's `KV_VALUE_MAX` 1024-byte value buffer is `applied:false` plus a
//!   warning, a `nodeID`
//!   above `i32::MAX` is `-32602` (the bridge would truncate it to a different node), and an
//!   empty `class`/`targetname` filter is `-32602` (brief items 13, 15);
//! * **every undoable write's reply carries `doc: {id, topSerial, undoDepth}`** (m2.17: the
//!   document state the write left behind, read inside the same dispatch — `with_doc`), which is
//!   the same state a `doc.active` of that moment would answer. The mock builds it after the op
//!   and after the concurrent-user seam has fired, so a test can tell a write's own single group
//!   from anything that moved inside its window. `undo.redo` is implemented by the bridge only
//!   (no server tool calls it) and the mock does not serve it.
//!
//! Geometry (WP-B) mirrors `geo_dispatch` + `geom_parse` + the three mesh ops: the same gate
//! order (editable -> `expectDoc` -> op), the same parameter refusals and texts, the same reply
//! fields (`nodeID`, `class`, `origin`, `vertexSlots`, `faceSlots`, `inputVertices`, `faces`,
//! `materials`, the bridge's own `undoLabel`) and one undo group per op whose snapshot restores
//! the geometry. `origin` is the geometry's bounds centre — what Hammer's `InitializeFromPolygonGroup`
//! does to the pivot. Two deliberate simplifications: the mock does **not** weld coincident
//! vertices (`vertexSlots` is the sent vertex count, like the POC's mock), and a mesh node with no
//! stored geometry has *no* faces (`mesh.setGeometry` stores what it is given and never needs the
//! old geometry; `mesh.setMaterial` then answers the bridge's `-32603 "mesh has no faces"`).
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use super::State;
use crate::mock::MOCK_ADDON;

mod selection;
pub use self::selection::SelectionState;

/// The twenty methods the hammer world serves (advertised in hello when the world is enabled).
pub const HAMMER_METHODS: [&str; 20] = [
    "entity.create",
    "entity.setKeyValues",
    "entity.get",
    "entity.find",
    "edit.batch",
    "node.list",
    "node.delete",
    "undo.undo",
    "map.save",
    "map.reload",
    "map.open",
    "doc.active",
    "mesh.create",
    "mesh.setGeometry",
    "mesh.setMaterial",
    // m2.18 (WP-R2): node.setTransform is also the advertised m2.18 build marker (WP-R1's
    // create_props checks it before sending any scales-bearing prop).
    "node.setTransform",
    "entity.addConnections",
    "entity.removeConnections",
    // m2.19 (WP-T): the brush-entity (trigger) tie/untie pair. `entity.get` gained the
    // `children` array in the same build (see `op_entity_get`); it is not a method, so it has no
    // row here — the marker for the m2.19 gate is `entity.tieMeshes`, exactly like m2.18's
    // `node.setTransform`.
    "entity.tieMeshes",
    "entity.untie",
];

/// The classes `entity.create` accepts (WP-M registry; anything else -> `-32006 UnknownClass`).
/// WP-T: `trigger_once` / `trigger_multiple` are here because `entity.tieMeshes` resolves its
/// `class` through the same node factory — a trigger is an ordinary FGD class the factory
/// creates, and the m2.19 tie would refuse the whole trigger block's classes without them.
pub const ENTITY_CLASSES: [&str; 26] = [
    "info_target",
    "info_player_counterterrorist",
    "info_player_terrorist",
    "light_omni2",
    "func_buyzone",
    "prop_dynamic",
    "prop_static",
    "prop_physics",
    "logic_relay",
    "point_script", // I7 player_model: the cs_script entity (a real csgo.fgd PointClass)
    "info_particle_system",
    "trigger_once",
    "trigger_multiple",
    // The sound classes (WP-S4): the real bridge's CMapNodeFactory registry carries every FGD
    // class, including the tags(Sound) ones — `entity.create` of snd_event_point/sphere/box,
    // env_soundscape and the logic helpers was verified live on m2.19 (re-snd.md R4). The mock
    // list stood in for that registry and simply predated them.
    "snd_event_point",
    "snd_event_sphere",
    "snd_event_alignedbox",
    "snd_event_orientedbox",
    "env_soundscape",
    "env_soundscape_proxy",
    "logic_auto",
    "logic_timer",
    // The FIX-E family tests need the real-FGD relatives of the six kinds (additive only):
    // snd_soundscape (base env_soundscape), env/snd_soundscape_triggerable,
    // snd_event_box_helper and snd_event_param are all real core/base.fgd classes the live
    // registry carries.
    "snd_soundscape",
    "env_soundscape_triggerable",
    "snd_soundscape_triggerable",
    "snd_event_box_helper",
    "snd_event_param",
];

/// The `ENTITY_CLASSES` rows that are FGD `@SolidClass`es (gamedata type 5 live): a brush entity
/// can only be one of these, so `entity.tieMeshes`'s `entityID` form takes them and its `class`
/// form refuses anything else with `-32602 "not a solid (brush) class"` — the bridge's defensive
/// `gdType != 5` check after `CreateNodeByName`. The mock has no FGD, so this list stands in for
/// the gamedata-kind lookup. `func_buyzone` is solid but not a trigger; both are brush classes.
pub const SOLID_CLASSES: [&str; 3] = ["func_buyzone", "trigger_once", "trigger_multiple"];

/// One node of the mock document. `class` is the walk class (`"world"|"entity"|"mesh"|"group"`),
/// `classname` the FGD class it carries (`"worldspawn"` for [`MockNode::world`]).
///
/// `parent` (WP-T, m2.19) is the m2.19 tie's tree edge: the nodeID of the `CMapEntity` a mesh was
/// tied to (`entity.tieMeshes` sets it, `entity.untie` clears it), `None` for a world node. The
/// bridge models the same edge as the entity's child handles (`HOFF_NODE_CHILD_COUNT/HANDLES`,
/// what `entity.get.children` reads), and the mock's flat node list carries it per node so a test
/// can seed a tied mesh with [`MockNode::with_parent`].
#[derive(Debug, Clone)]
pub struct MockNode {
    pub node_id: i64,
    pub class: String,
    pub classname: String,
    /// Mirrors the `targetname` keyvalue; it is what `entity.find` filters on.
    pub targetname: Option<String>,
    pub origin: [f64; 3],
    pub angles: [f64; 3],
    /// All entity keyvalues, always stored as text (`entity.create`/`setKeyValues` coerce:
    /// numbers keep their JSON text, bools become `"1"`/`"0"` — plan F1).
    pub keyvalues: BTreeMap<String, String>,
    pub locked: bool,
    /// m2.19: the entity this node is tied to (`entity.tieMeshes` writes it, `entity.untie`
    /// clears it). `None` = a world node.
    pub parent: Option<i64>,
}

impl MockNode {
    /// A `class:"entity"` node; `keyvalues["targetname"]` also seeds `targetname`.
    pub fn entity(
        id: i64,
        classname: impl Into<String>,
        origin: [f64; 3],
        kv: BTreeMap<String, String>,
    ) -> Self {
        MockNode {
            node_id: id,
            class: "entity".into(),
            classname: classname.into(),
            targetname: kv.get("targetname").cloned(),
            origin,
            angles: [0.0; 3],
            keyvalues: kv,
            locked: false,
            parent: None,
        }
    }

    /// The worldspawn node (nodeID 1) every document starts with.
    pub fn world() -> Self {
        MockNode {
            node_id: 1,
            class: "world".into(),
            classname: "worldspawn".into(),
            targetname: None,
            origin: [0.0; 3],
            angles: [0.0; 3],
            keyvalues: BTreeMap::new(),
            locked: false,
            parent: None,
        }
    }

    /// m2.19 builder: a mesh node already tied to `parent` (the state a previous
    /// `entity.tieMeshes` leaves behind, and what `entity.get.children` reports).
    pub fn with_parent(mut self, parent: i64) -> Self {
        self.parent = Some(parent);
        self
    }
}

/// One live I/O connection of an entity node (the m2.18 `CMapEntityIONode` 80-byte vector entry;
/// the names are the ones the `.vmap` reader and `entity.get` report). `targetType` is not stored:
/// every connection the bridge writes carries the UI's `7` (`re-props.md` Q3).
#[derive(Debug, Clone, PartialEq)]
pub struct MockConnection {
    pub output: String,
    pub target: String,
    pub input: String,
    /// `overrideParam`; the empty string means "no parameter".
    pub param: String,
    pub delay: f64,
    /// `-1` = fire whenever the output fires.
    pub times_to_fire: i64,
}

/// One mesh's geometry as the mock holds it (bridge `GGeom` after `InitializeFromPolygonGroup`).
/// Materials are resolved per face (the bridge stores `-1` = default, which reads back as the
/// base material).
#[derive(Debug, Clone, PartialEq)]
pub struct MockMesh {
    pub vertices: Vec<[f64; 3]>,
    pub faces: Vec<Vec<usize>>,
    /// One material path per face, in face order (never empty-handed: a mesh created without
    /// `faceMaterials` holds the base material on every face).
    pub materials: Vec<String>,
}

/// One undo group of the world's history. `snapshot: None` marks a keyvalue-only group
/// (`MCP: set keyvalues` — the standalone form and any batch without a create op, the label the
/// bridge's `batch_has_create` gives it) that pops without restoring — F2.
///
/// `serial` is the value the monotonic counter had when the group was recorded: the **top**
/// group's serial (`topSerial`) goes down again when it is popped, while the counter never does
/// (`serial-probe.log`; review-4 R4-3).
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub label: String,
    pub serial: u64,
    pub snapshot: Option<Snapshot>,
}

/// A mesh node's own transform as `mesh.get` reports it (`origin`, `angles` `[pitch, yaw, roll]`
/// degrees, `scales`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshTransform {
    pub origin: [f64; 3],
    pub angles: [f64; 3],
    pub scales: [f64; 3],
}

/// What a snapshot group restores: the node list **and** the geometry (a mesh created, re-shaped or
/// re-materialed by one of the geometry ops), plus the m2.18 per-node state a prop write touches
/// (I/O connections, object-properties scales).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub nodes: Vec<MockNode>,
    pub meshes: BTreeMap<i64, MockMesh>,
    pub connections: BTreeMap<i64, Vec<MockConnection>>,
    pub node_scales: BTreeMap<i64, [f64; 3]>,
}

/// The mock document world served by the hammer built-ins.
#[derive(Debug)]
pub struct HammerWorld {
    pub nodes: Vec<MockNode>,
    /// Geometry per mesh node id (`mesh.create`/`mesh.setGeometry` write it).
    pub meshes: BTreeMap<i64, MockMesh>,
    /// m2.18: the live I/O connections per entity node id (`entity.addConnections` appends,
    /// `entity.removeConnections` removes; `entity.get` reports them). A missing entry = none.
    pub connections: BTreeMap<i64, Vec<MockConnection>>,
    /// m2.18: the object-properties scales per node id (`node.setTransform` writes them; a missing
    /// entry is the identity `[1,1,1]` — like a live node that never had them written).
    pub node_scales: BTreeMap<i64, [f64; 3]>,
    pub undo: Vec<UndoEntry>,
    /// The redo stack: groups a Hammer-UI undo ([`HammerWorld::user_undo`]) popped. A redo
    /// ([`HammerWorld::user_redo`]) puts the group's state back as a **new** record with the next
    /// serial (see that function: what Hammer's own redo does is unverified — the bridge has no
    /// `undo.redo` — and the used-up serial on undo points this way). Any new group (`push_undo`)
    /// clears it, as Hammer's own redo stack is invalidated by a new operation.
    pub redo: Vec<UndoEntry>,
    /// `doc.active`'s `id` is `d{doc_seq}` (constant 1 = `d1`, like the fixture doc).
    pub doc_seq: i64,
    /// Bumped by every mutation (what `doc.active`/`expectDoc` use as the fingerprint epoch).
    pub epoch: i64,
    /// Test seam: when set, MCP mutations do NOT bump [`HammerWorld::epoch`], like the real
    /// bridge's structural `doc_fingerprint` (session/root/path), which an edit does not move.
    pub edits_keep_epoch: bool,
    pub dirty: bool,
    pub editable: bool,
    pub new_file: bool,
    /// `map.save`'s path (empty = untitled) — seeded from the builder's `.doc()` object.
    pub path: String,
    /// The addon-relative paths (lower case, `maps/b.vmap`) the mock's host really has: `map.open`
    /// answers `-32014 AssetNotFound` for any other one (the bridge's host gate) and always has the
    /// active document's own file (seeded from `path`; `.hammer_map_files` adds more). Matched
    /// case-insensitively, like the bridge's host + session comparisons.
    pub map_files: Vec<String>,
    /// The Windows paths of the sessions the mock's editor holds, in open order (review M2). Live,
    /// the app keeps every session in its vector, so `map.open` of a file one of them already holds
    /// answers `{opened:false, existing:true, active:<that session is the active one>}`; the mock
    /// used to model a single document and answered `opened:true` for the A -> B -> A sequence.
    /// Seeded with the builder document's own path (an untitled `new_file` document holds none).
    pub sessions: Vec<String>,
    /// Monotonic undo `topSerial`.
    next_serial: u64,
    /// Handle tokens ("n1", "n2", ...) issued by `entity.create` (batch-scoped in the mock).
    handle_seq: u64,
    /// M1 knob: how many faces Hammer "welds away" from every `mesh.create`/`mesh.setGeometry`
    /// request (the mock has no welding of its own, and review M1's failure needs a geometry whose
    /// live face count differs from the one sent). The reply's `faces` is the reduced count and the
    /// stored geometry holds only the faces that "survived", exactly like a real weld dropping a
    /// polygon.
    pub drop_faces: usize,
    /// review-2 M3/L1 test seam: a Hammer UI edit the MCP server cannot see (its own undo group:
    /// `topSerial`/`undoDepth` move and the document goes dirty, the epoch does not — a Hammer
    /// edit does not move the bridge's document fingerprint).
    user_edits: u64,
    /// review-3 NEW-1 test seams: Hammer-UI undos/redos made through [`HammerWorld::user_undo`] /
    /// [`HammerWorld::user_redo`] (assertions), and the geometry writes seen so far for the
    /// concurrent-group seam below.
    user_undos: u64,
    user_redos: u64,
    geometry_writes: u64,
    /// review-3 NEW-3 test seam: make one Hammer-UI group right after the Nth successful
    /// `mesh.create`/`mesh.setGeometry` reply (a user drag or a second hammer-mcp process landing
    /// between a write and its post-write state read, review-4 R4-1 step 3). `None` (the default)
    /// makes none.
    group_after_write: Option<u64>,
    /// review-2 L2 test seam: switch the document identity (`docSeq`/`epoch`, like a tab
    /// opening a second session live) right after the Nth `doc.active` read, so a call whose own
    /// read precedes a second one hits the window the L2 fix closes.
    doc_reads: u64,
    switch_after_reads: Option<u64>,
    /// m3.12: whether the world serves `mesh.get` (the builder's `hammer_mesh_get`; `false` models a
    /// bridge older than m3.12 — the method is also left out of the advertisement).
    mesh_get: bool,
    /// m3.14: whether the world serves `mesh.setFaceTexture` (the builder's `hammer_face_texture`;
    /// `false` models a bridge older than m3.14 - the method is left out of the advertisement).
    face_texture: bool,
    /// m3.14 test seam: how `mesh.setFaceTexture` fails (`"pre"`, `"panic"`, `"mismatch"`).
    face_texture_fail: Option<String>,
    /// m3.14: each face's texture state, `(nodeID, face) -> [axisU(4), axisV(4), scale(2)]`;
    /// a face without an entry reads back as [`DEFAULT_FACE_TEX`]. Not part of the undo snapshot
    /// (the mock counts the undo group; it does not restore texture values).
    face_tex: BTreeMap<(i64, usize), [f64; 10]>,
    /// m3.12: the transform `mesh.get` reports per mesh node and converts the stored (world, as
    /// sent) geometry back to local with; a missing entry is `(node.origin, [0,0,0], [1,1,1])` —
    /// what a rebuilt mesh reads back as (the bridge's `geo_reset_mesh` zeroes angles/scales).
    mesh_xf: BTreeMap<i64, MeshTransform>,
    /// m3.12: `mesh.get` refuses every mesh as too large for one reply (the bridge's `-32602`
    /// with the counts), the shape a mesh of tens of thousands of vertices gets.
    mesh_too_large: bool,
    /// m2.17: whether an undoable write's reply carries `doc` (the document state it left behind).
    /// `true` is the m2.17 bridge; `false` models an **older** bridge, whose write replies have no
    /// tail — the case hammer-mcp's fallback `doc.active` read still covers.
    write_doc: bool,
    /// m2.18: whether `entity.setKeyValues` (standalone) records real value undo
    /// (`RecordNodeTree`, BRIDGE-M2.18-SPEC §1): the reply says `valueUndo:"recorded"` and the
    /// popped group restores the values. The default is `true` — this mock advertises the m2.18
    /// marker (`node.setTransform`), and no real m2.18-advertising build answers
    /// `valueUndo:"group-only"` (review w2b #5), so a `false` here models an **older**
    /// method-set (m2.15..m2.17) build: the advertised methods lose `node.setTransform`
    /// (`MockBuilder::methods`) together with the knob, and the reply carries
    /// `valueUndo:"group-only"` with a snapshot-less group.
    value_undo_recorded: bool,
    /// review-2 W2 #3 test seam: make the read-back of the Nth `entity.addConnections` entry fail,
    /// so the reply carries `{"readback":false,...}` for it (the bridge's own cross-check: the
    /// entry was handed to AddConnection but the vector did not confirm it - maybe-executed).
    /// `None` (the default) confirms every entry.
    drop_conn_readback: Option<i64>,
    /// m3.12: the selection model (`None` = the bridge does not advertise `selection.*`).
    selection: Option<selection::SelectionModel>,
}

impl HammerWorld {
    pub(crate) fn new(
        nodes: Vec<MockNode>,
        path: String,
        dirty: bool,
        editable: bool,
        new_file: bool,
        map_files: Vec<String>,
    ) -> Self {
        // the document's own session: an untitled document holds no path, so it holds no session
        // either (review M2)
        let sessions = if new_file || path.is_empty() {
            Vec::new()
        } else {
            vec![path.clone()]
        };
        HammerWorld {
            nodes,
            meshes: BTreeMap::new(),
            connections: BTreeMap::new(),
            node_scales: BTreeMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            doc_seq: 1,
            epoch: 1,
            edits_keep_epoch: false,
            dirty,
            editable,
            new_file,
            path,
            map_files,
            sessions,
            next_serial: 0,
            handle_seq: 0,
            drop_faces: 0,
            user_edits: 0,
            user_undos: 0,
            user_redos: 0,
            geometry_writes: 0,
            group_after_write: None,
            doc_reads: 0,
            switch_after_reads: None,
            mesh_get: true,
            face_texture: true,
            face_texture_fail: None,
            face_tex: BTreeMap::new(),
            mesh_xf: BTreeMap::new(),
            mesh_too_large: false,
            write_doc: true,
            value_undo_recorded: true,
            drop_conn_readback: None,
            selection: None,
        }
    }

    /// m3.12: make `mesh.get` answer the "mesh too large for one reply" refusal.
    pub fn set_mesh_too_large(&mut self, v: bool) {
        self.mesh_too_large = v;
    }

    /// m3.14: serve (or not) `mesh.setFaceTexture`.
    pub fn set_face_texture(&mut self, v: bool) {
        self.face_texture = v;
    }

    /// m3.14: the failure `mesh.setFaceTexture` injects (see `MockBridge::set_hammer_face_texture_fail`).
    pub fn set_face_texture_fail(&mut self, v: Option<String>) {
        self.face_texture_fail = v;
    }

    /// m3.12: serve (or not) `mesh.get`.
    pub fn set_mesh_get(&mut self, v: bool) {
        self.mesh_get = v;
    }

    /// m3.12: the transform `mesh.get` reports for `node_id` (the stored world geometry is
    /// converted to local with it, so a caller applying the same transform gets the stored world
    /// geometry back).
    pub fn set_mesh_transform(&mut self, node_id: i64, xf: MeshTransform) {
        self.mesh_xf.insert(node_id, xf);
    }

    /// m2.17: `false` makes every undoable write's reply drop the `doc` tail — the shape a bridge
    /// older than m2.17 answers, which hammer-mcp must still serve with its own `doc.active` read.
    pub fn set_write_doc(&mut self, v: bool) {
        self.write_doc = v;
    }

    /// m2.18: model the `entity.setKeyValues` change (`RecordNodeTree`): the reply says
    /// `valueUndo:"recorded"` and the popped `"MCP: set keyvalues"` group restores the values.
    /// This is the mock's default (review w2b #5: an m2.18-advertising world answers nothing
    /// else); `false` models a pre-m2.18 build — pair it with `.methods(...)` without
    /// `node.setTransform`, like the capabilities that build would advertise.
    pub fn set_value_undo_recorded(&mut self, v: bool) {
        self.value_undo_recorded = v;
    }

    /// M1 knob: Hammer "welds away" up to `drop_faces` faces of every geometry request (the mock's
    /// stand-in for a real weld/degenerate-polygon drop: the reply's `faces` and the stored geometry
    /// both lose them, so the live face count no longer matches the request's). `0` (the default)
    /// keeps every face.
    pub fn set_drop_faces(&mut self, drop: usize) {
        self.drop_faces = drop;
    }

    /// review-2 W2 #3 seam: the add read-back for wire entry `at` fails, so the reply carries
    /// `{"readback":false,...}` for it (the entry still lands in the vector - the bridge cannot
    /// prove it did not - hence "maybe-executed"). `None` (the default) confirms every entry.
    pub fn set_drop_conn_readback(&mut self, at: Option<i64>) {
        self.drop_conn_readback = at;
    }

    /// review-2 M3/L1 test seam: one Hammer UI edit the MCP server did not make and cannot see.
    /// Its own undo group (`topSerial`/`undoDepth` move), the document goes dirty (the live fact:
    /// `doc.active`'s `ChangedSinceSave` says true after any edit, undone past the save point
    /// included), and the epoch stays (a Hammer edit does not move the document fingerprint).
    pub fn user_edit(&mut self, label: &str) {
        let snapshot = self.snapshot();
        self.push_undo(label, Some(snapshot));
        self.dirty = true;
        self.user_edits += 1;
    }

    /// How many UI edits were made through [`HammerWorld::user_edit`] (test assertions).
    pub fn user_edits(&self) -> u64 {
        self.user_edits
    }

    /// review-3 NEW-1 test seam: the user presses Ctrl+Z in Hammer. The top group is popped and its
    /// snapshot restored, exactly like the bridge's `undo.undo`. Two live facts ride it:
    ///
    /// * the undo **consumes one serial** — the counter is never rewound *and* it advances, so the
    ///   next group's serial skips one (`serial-probe.log`: create, create, undo, create reports
    ///   `1, 2, 1, 4`; review-4 R4-3);
    /// * `topSerial` (the serial of the group now on top) therefore goes **down** with the pop while
    ///   the counter behind it does not, which is what makes a per-entry serial prediction unsafe
    ///   and the sticky state comparison (review-4 R4-1) necessary.
    ///
    /// The document goes dirty (Hammer's `ChangedSinceSave` is true after any edit, an undo past
    /// the save point included) and the fingerprint epoch stays (a Hammer UI undo is not a
    /// document-identity change). The popped group moves to [`HammerWorld::redo`] with the state
    /// *after* it, so [`HammerWorld::user_redo`] can restore it. `false` when the stack was empty.
    pub fn user_undo(&mut self) -> bool {
        let Some(mut entry) = self.undo.pop() else {
            return false;
        };
        // the undo itself uses up one serial (live, `serial-probe.log`)
        self.next_serial += 1;
        // what a redo restores: the state the group produced, which is the state right now
        let after = self.snapshot();
        let mut redo = entry.clone();
        redo.snapshot = Some(after);
        if let Some(snapshot) = entry.snapshot.take() {
            self.nodes = snapshot.nodes;
            self.meshes = snapshot.meshes;
            self.connections = snapshot.connections;
            self.node_scales = snapshot.node_scales;
        }
        self.redo.push(redo);
        self.dirty = true;
        self.user_undos += 1;
        true
    }

    /// review-3 NEW-1 test seam: the user presses Ctrl+Y/Ctrl+Shift+Z. The group's state comes back,
    /// but as a **new** group with the next serial from the counter (`next_serial += 1`) — the
    /// redo that Hammer really performs is *unverified* (the bridge has no `undo.redo`, review-4
    /// R4-3 gap 2): re-pushing the popped record with its old serial would make the document state
    /// match what it was before the undo, while live's used-up serial on undo (`serial-probe.log`)
    /// suggests Hammer builds the redo record at the *next* serial. The conservative reading is
    /// modelled: the state moves, so the sticky mesh-cache rule (review-4 R4-1) sees a difference
    /// and drops the entries — `a_user_undo_then_redo_misses_the_mesh_cache` pins it. `false` when
    /// there is nothing to redo.
    pub fn user_redo(&mut self) -> bool {
        let Some(mut entry) = self.redo.pop() else {
            return false;
        };
        if let Some(snapshot) = &entry.snapshot {
            self.nodes = snapshot.nodes.clone();
            self.meshes = snapshot.meshes.clone();
            self.connections = snapshot.connections.clone();
            self.node_scales = snapshot.node_scales.clone();
        }
        // a redo is a new record off the same monotonic counter (unverified live, see above)
        self.next_serial += 1;
        entry.serial = self.next_serial;
        self.undo.push(entry);
        self.user_redos += 1;
        true
    }

    /// How many Hammer-UI undos/redos ran through the review-3 NEW-1 seams.
    pub fn user_undos(&self) -> u64 {
        self.user_undos
    }

    pub fn user_redos(&self) -> u64 {
        self.user_redos
    }

    /// review-3 NEW-3 test seam: make one Hammer-UI group right after the `n`th successful
    /// `mesh.create`/`mesh.setGeometry` reply (`None` turns it off). It models a concurrent group —
    /// the user's own drag, or a second hammer-mcp process sharing the bridge — landing between this
    /// server's write and the `doc.active` read that verifies it, so the state the tool reads is no
    /// longer its own one-group step.
    pub fn set_group_after_write(&mut self, n: Option<u64>) {
        self.group_after_write = n;
    }

    /// One geometry write landed: run the [`HammerWorld::group_after_write`] seam when it is this
    /// write's turn.
    fn after_geometry_write(&mut self) {
        self.geometry_writes += 1;
        if self.group_after_write == Some(self.geometry_writes) {
            self.user_edit("Hammer: user drag (concurrent group)");
        }
    }

    /// review-2 L2 test seam: after the Nth upcoming `doc.active` read the document identity
    /// switches (a new `docSeq`/`epoch`, like opening a second session live): `hammer-doc-switch-after`.
    pub fn set_doc_switch_after(&mut self, reads: Option<u64>) {
        self.switch_after_reads = reads;
    }

    /// Apply the [`HammerWorld::drop_faces`] knob to one parsed request: the last faces are dropped
    /// from the geometry Hammer "built" (never all of them — the bridge's `geo_build` fails with
    /// "Hammer created no faces" instead).
    fn weld_drop(&self, geom: &mut MockGeom) {
        if self.drop_faces == 0 || geom.faces.len() <= self.drop_faces {
            return;
        }
        let kept = geom.faces.len() - self.drop_faces;
        geom.faces.truncate(kept);
        geom.materials.truncate(kept);
    }

    /// A snapshot for the next undo group (node list + geometry + the m2.18 per-node state).
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            nodes: self.nodes.clone(),
            meshes: self.meshes.clone(),
            connections: self.connections.clone(),
            node_scales: self.node_scales.clone(),
        }
    }

    /// Push one undo group (`serial` = the monotonic top serial). A new group invalidates the redo
    /// stack, as it does in Hammer.
    fn push_undo(&mut self, label: &str, snapshot: Option<Snapshot>) {
        self.next_serial += 1;
        self.redo.clear();
        self.undo.push(UndoEntry {
            label: label.into(),
            serial: self.next_serial,
            snapshot,
        });
    }

    /// The `doc.active` doc object, exactly like `mcp_bridge.c`'s `doc.active` arm.
    fn doc_json(&self) -> Value {
        let (undo_name, undo_depth, top_serial) = match self.undo.last() {
            None => (json!(null), 0, json!(0)),
            Some(e) => (json!(e.label), self.undo.len() as i64, json!(e.serial)),
        };
        json!({
            "id": format!("d{}", self.doc_seq),
            "path": if self.new_file { json!(null) } else { json!(self.path) },
            "dirty": self.dirty,
            "active": true,
            "editable": self.editable,
            "notEditableReason": if self.editable { "ok" } else { "not_editable" },
            "epoch": self.epoch,
            "docType": "vmap",
            "isNewFile": self.new_file,
            "undoName": undo_name,
            "undoDepth": undo_depth,
            "topSerial": top_serial,
        })
    }
}

fn err(code: i64, message: &str, data: Value) -> Value {
    json!({"code": code, "message": message, "data": data})
}

/// `-32602 InvalidParams {reason}` (the built-ins' rejection shape, `op_err`'s).
fn invalid_params(reason: impl Into<String>) -> Value {
    err(-32602, "InvalidParams", json!({"reason": reason.into()}))
}

/// m2.17: the document state an undoable write's **own reply** carries (`doc` in the result), read
/// on the main thread inside the same dispatch as the op. The mock builds it from the same source
/// `doc_json` reads, so a reply's `doc` can never disagree with a `doc.active` of the same moment.
fn doc_state_json(w: &HammerWorld) -> Value {
    let (top_serial, depth) = match w.undo.last() {
        None => (json!(0), 0),
        Some(e) => (json!(e.serial), w.undo.len() as i64),
    };
    json!({
        "id": format!("d{}", w.doc_seq),
        "topSerial": top_serial,
        "undoDepth": depth,
    })
}

/// Add the m2.17 `doc` state to a successful write result (a non-object result is left alone, and
/// so is every reply when [`HammerWorld::set_write_doc`] models an older bridge). Called after
/// every mutation the op made — including the mock's concurrent-user-group seams, which inject
/// *before* the reply is built exactly like a live Hammer group landing inside the write's own
/// dispatch would (`user_group_after_write`).
fn with_doc(w: &HammerWorld, v: Value) -> Value {
    if !w.write_doc {
        return v;
    }
    match v {
        Value::Object(mut obj) => {
            obj.insert("doc".into(), doc_state_json(w));
            Value::Object(obj)
        }
        other => other,
    }
}

/// The same gate for the selection model's own file.
fn expect_doc_ok_for_selection(w: &HammerWorld, params: &Value) -> Result<(), Value> {
    expect_doc_ok(w, params)
}

/// `-32004 DocumentChanged` when a mutator's `expectDoc` misses the fingerprint (`expect_doc_ok`).
fn changed(reason: &str) -> Value {
    err(-32004, "DocumentChanged", json!({"reason": reason}))
}

/// `expectDoc {id:"dN", epoch}` must match the world's fingerprint (mirror `expect_doc_ok`).
fn expect_doc_ok(w: &HammerWorld, params: &Value) -> Result<(), Value> {
    let Some(ed) = params.get("expectDoc") else {
        return Err(changed("missing"));
    };
    if !ed.is_object() {
        return Err(changed("missing"));
    }
    let Some(id) = ed.get("id").and_then(Value::as_str) else {
        return Err(changed("missing_id"));
    };
    let Some(epoch) = ed.get("epoch").and_then(Value::as_i64) else {
        return Err(changed("missing_epoch"));
    };
    let digits = id
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .fold(0u64, |acc, b| acc * 10 + u64::from(b - b'0'));
    if digits as i64 != w.doc_seq {
        return Err(changed("doc_id"));
    }
    if w.epoch != epoch {
        return Err(changed("epoch"));
    }
    Ok(())
}

/// Every mutation marks the document dirty and bumps the fingerprint epoch.
fn changed_epoch(w: &mut HammerWorld) {
    w.dirty = true;
    if !w.edits_keep_epoch {
        w.epoch += 1;
    }
}

/// The bridge's `sval[KV_VALUE_MAX + 1]`: a *string value longer than* `KV_VALUE_MAX` (1024) does
/// not fit its buffer plus the NUL, so a longer value is cut (or drops the keys after it) — the
/// mock refuses it the way the bridge does, `applied:false` plus a warning (brief item 15). The
/// Rust tool's own cap stays 511 (`hammer-mcp` `MAX_VALUE_BYTES`).
const MAX_VALUE_BYTES: usize = 1024;

/// One value coerced into the mock's uniform string storage (numbers as their JSON text,
/// bools as "1"/"0", plan §3.4); arrays/objects are not writable.
fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(true) => Some("1".into()),
        Value::Bool(false) => Some("0".into()),
        _ => None,
    }
}

/// A node id the bridge would truncate to a different node is refused like a bad parameter
/// (brief item 15: the bridge casts `nodeID` to `int`).
fn node_id_ok(id: i64) -> bool {
    (0..=i64::from(i32::MAX)).contains(&id)
}

fn node_of(w: &HammerWorld, id: i64) -> Option<&MockNode> {
    w.nodes.iter().find(|n| n.node_id == id)
}

/// The next free nodeID (`max(all)+1`, at least 2 — the world owns nodeID 1).
fn next_node_id(w: &HammerWorld) -> i64 {
    2.max(w.nodes.iter().map(|n| n.node_id).max().unwrap_or(1) + 1)
}

fn node_not_found(id: i64) -> Value {
    err(
        -32022,
        "NodeNotFound",
        json!({"reason": format!("nodeID {id} not found")}),
    )
}

/// `origin`/`angles` read per component like the bridge's `json_float3` (missing/garbage = 0).
fn num3(params: &Value, key: &str) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    if let Some(arr) = params.get(key).and_then(Value::as_array) {
        for (i, v) in arr.iter().take(3).enumerate() {
            out[i] = v.as_f64().unwrap_or(0.0);
        }
    }
    out
}

/// Apply `{"name":value}` pairs onto one node (`apply_keyvalues`): every writable kind is
/// coerced to text and applied (`true`); arrays/objects and values longer than the bridge's
/// `KV_VALUE_MAX` 1024-byte buffer answer `applied:false` plus the bridge's warning text.
/// Returns `(applied, warnings, previous)` — `previous` only carries meaning for `setKeyValues`.
fn apply_keyvalues(node: &mut MockNode, values: &Value) -> (Value, Value, Value) {
    let obj = values.as_object().cloned().unwrap_or_default();
    let mut applied = Map::new();
    let mut previous = Map::new();
    let mut warnings: Vec<Value> = vec![];
    for (name, v) in obj.iter() {
        // the keyvalue undo never restores (F2): remember what was there for the result only
        let had = node.keyvalues.get(name);
        previous.insert(name.clone(), had.map(|old| json!(old)).unwrap_or(Value::Null));
        match value_text(v) {
            Some(text) if text.len() > MAX_VALUE_BYTES => {
                applied.insert(name.clone(), json!(false));
                // the bridge's warning text (mcp_bridge.c `apply_keyvalues`)
                warnings.push(json!(format!(
                    "{name}: value longer than {MAX_VALUE_BYTES} bytes"
                )));
            }
            Some(text) => {
                if name == "targetname" {
                    node.targetname = Some(text.clone());
                }
                node.keyvalues.insert(name.clone(), text);
                applied.insert(name.clone(), json!(true));
            }
            None => {
                applied.insert(name.clone(), json!(false));
                warnings.push(json!(format!("{name}: value type not writable")));
            }
        }
    }
    (Value::Object(applied), json!(warnings), Value::Object(previous))
}

/// `num3` for a whole `[x, y, z]`, but m2.18-strict: the `entity.create`/`node.setTransform`
/// scale values are finite and each in `(0, 1000]` — 0, negative and non-finite scales are
/// refused with `InvalidParams` **before** anything is created (BRIDGE-M2.18-SPEC §2).
/// `None` when the parameter is absent or null.
fn scales3(params: &Value, key: &str) -> Result<Option<[f64; 3]>, Value> {
    let raw = match params.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let Some(axes) = raw.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid_params(format!(
            "{key}: [x, y, z] of numbers in (0, 1000]"
        )));
    };
    let mut out = [0.0f64; 3];
    for (axis, value) in axes.iter().enumerate() {
        match value.as_f64() {
            Some(number) if number.is_finite() && number > 0.0 && number <= 1000.0 => {
                out[axis] = number;
            }
            Some(number) => {
                return Err(invalid_params(format!(
                    "{key}[{axis}]: {number} is outside (0, 1000] (0, negative and non-finite \
                     scales are refused before anything is created)"
                )))
            }
            None => {
                return Err(invalid_params(format!(
                    "{key}[{axis}]: {value} is not a finite number"
                )))
            }
        }
    }
    Ok(Some(out))
}

/// `entity.create` op: the exact wire contract of `op_entity_create` — class registry gate,
/// "next nodeID", text-only keyvalue storage. Returns the result object and the handle just
/// issued (issuing only matters inside a batch; a standalone create's handle is dead on return).
fn op_create(w: &mut HammerWorld, params: &Value) -> Result<(Value, String), Value> {
    let Some(cls) = params
        .get("class")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    else {
        return Err(invalid_params("class"));
    };
    if !ENTITY_CLASSES.contains(&cls) {
        return Err(err(
            -32006,
            "UnknownClass",
            json!({"reason": "not in the CMapNodeFactory registry"}),
        ));
    }
    // m2.18 §2: the scales are read and bounded BEFORE the node is created (a refused scales
    // array must never leave a created node behind).
    let scales = scales3(params, "scales")?;
    let origin = num3(params, "origin");
    let angles = num3(params, "angles");
    let id = next_node_id(w);
    w.handle_seq += 1;
    let handle = format!("n{}", w.handle_seq);
    let mut node = MockNode {
        node_id: id,
        class: "entity".into(),
        classname: cls.to_string(),
        targetname: None,
        origin,
        angles,
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    };
    let mut result = json!({
        "handle": handle,
        // the created node's id, exactly what node.list/entity.get address it by (the bridge's
        // create reply carries it since the W2 review: the handle is batch-scoped)
        "nodeID": id,
        "class": cls,
        "origin": origin,
        "undoLabel": "MCP: create entity",
    });
    // m2.18 §2: the reply echoes the scales the entity vslot applied (the read-back shape the
    // `node.setTransform` recipe uses: the fields' values, not the request's). The echo is
    // **always** present like the real bridge's (protocol-diff §7 entity.create row: `[1,1,1]`
    // when the request sent none; review w2b #6 — the mock used to drop the member and taught
    // readers that an unscaled create has no `scales` key).
    result["scales"] = json!(scales.as_ref().copied().unwrap_or([1.0, 1.0, 1.0]));
    if let Some(kvs) = params.get("keyValues").filter(|v| v.is_object()) {
        let (applied, warnings, _) = apply_keyvalues(&mut node, kvs);
        result["keyValues"] = json!({"applied": applied, "warnings": warnings});
    }
    changed_epoch(w);
    w.nodes.push(node);
    if let Some(scales) = scales {
        // m2.18 §2: the create's scales are a scale write like `node.setTransform`'s — the
        // object-properties map (a later `node.list` / `entity.get` round-trip reads it).
        w.node_scales.insert(id, scales);
    }
    Ok((result, handle))
}

/// `entity.setKeyValues` op (standalone and batch `op_set_keyvalues`): exactly one of
/// `handle`/`nodeID`, the handle only valid inside the same `edit.batch` request, text
/// coercion, `previous` and the deliberate `"valueUndo":"group-only"` marker.
fn op_set_keyvalues(
    w: &mut HammerWorld,
    params: &Value,
    handles: &[(String, i64)],
) -> Result<Value, Value> {
    let handle = params.get("handle");
    let node_id = params.get("nodeID");
    if handle.is_some() == node_id.is_some() {
        return Err(invalid_params(
            "exactly one of handle or nodeID is required",
        ));
    }
    let id = if let Some(h) = handle {
        // mock handles never live across requests: only `$<index>` into this request's
        // issue table is resolvable (a literal "nN" or a stale "$N" -> unknown_handle)
        let Some(token) = h.as_str() else {
            return Err(invalid_params("handle: a string is required"));
        };
        let resolved = token
            .strip_prefix('$')
            .and_then(|rest| rest.parse::<usize>().ok())
            .and_then(|n| handles.get(n))
            .map(|(_, id)| *id);
        let Some(id) = resolved else {
            return Err(err(-32005, "StaleHandle", json!({"reason": "unknown_handle"})));
        };
        id
    } else {
        let Some(id) = node_id.and_then(Value::as_i64) else {
            return Err(invalid_params("nodeID"));
        };
        if !node_id_ok(id) {
            return Err(invalid_params(format!(
                "nodeID {id} is above the bridge's int range"
            )));
        }
        id
    };
    let Some(node) = w.nodes.iter_mut().find(|n| n.node_id == id) else {
        return Err(node_not_found(id));
    };
    if node.class != "entity" {
        return Err(invalid_params("not an entity"));
    }
    if node.locked {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": format!("nodeID {id} is locked")}),
        ));
    }
    let Some(values) = params.get("values").filter(|v| v.is_object()) else {
        return Err(invalid_params("values"));
    };
    let (applied, warnings, previous) = apply_keyvalues(node, values);
    changed_epoch(w);
    // m2.18: with `RecordNodeTree` the group restores the values (the reply says "recorded");
    // the pre-m2.18 build answers "group-only" and pops the group without a restore (F2). The
    // default is the m2.18 shape (review w2b #5); pair a `false` knob with a method set that
    // drops `node.setTransform`, like the older build's capabilities.
    let value_undo = if w.value_undo_recorded {
        "recorded"
    } else {
        "group-only"
    };
    Ok(json!({
        "applied": applied,
        "warnings": warnings,
        "previous": previous,
        "valueUndo": value_undo,
    }))
}

/// `entity.get {nodeID}` (plan §3.1): the live read the other arms are built on. m2.18: for an
/// entity the reply carries the live I/O connection vector with the `.vmap` reader's names
/// (`outputName`, `targetType`, `targetName`, `inputName`, `overrideParam`, `delay`,
/// `timesToFire`) — an empty array for a node without connections, like the bridge's vector read
/// (`re-props.md` Q3). Non-entities are refused before this (`-32602 not an entity`), so the
/// bridge's "only for CMapEntity, else `null`" case cannot be reached.
///
/// m2.19 (WP-T): the reply also carries `children` — the nodes tied to this entity, in node-list
/// order as `{nodeID, class}` rows, capped at [`MAX_CHILDREN`] with `childrenTruncated` set when
/// the cap cut something. That is what `op_tie_meshes`/`op_untie` maintain through
/// [`MockNode::parent`] (the bridge reads the same fact from
/// `HOFF_NODE_CHILD_COUNT/CHILD_HANDLES`, `geo_walk`'s source); an empty array is the normal
/// answer for a point entity (the m2.19 spec's `null` is the bridge's *unreadable* child array,
/// a failure this mock cannot produce).
fn op_entity_get(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let Some(id) = params.get("nodeID").and_then(Value::as_i64) else {
        return Err(invalid_params("nodeID"));
    };
    if !node_id_ok(id) {
        return Err(invalid_params(format!(
            "nodeID {id} is above the bridge's int range (it would truncate to a different node)"
        )));
    }
    let Some(node) = node_of(w, id) else {
        return Err(node_not_found(id));
    };
    if node.class != "entity" {
        return Err(invalid_params("not an entity"));
    }
    let keyvalues: Value = node
        .keyvalues
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect::<Map<_, _>>()
        .into();
    let connections: Value = w
        .connections
        .get(&id)
        .map(|list| {
            list.iter()
                .map(|c| {
                    json!({
                        "outputName": c.output,
                        "targetType": 7,
                        "targetName": c.target,
                        "inputName": c.input,
                        "overrideParam": c.param,
                        "delay": c.delay,
                        "timesToFire": c.times_to_fire,
                    })
                })
                .collect::<Vec<Value>>()
        })
        .unwrap_or_default()
        .into();
    let (children, children_truncated) = child_rows(w, id);
    Ok(json!({
        "nodeID": id,
        "classname": node.classname,
        "targetname": node.targetname,
        "origin": node.origin,
        "angles": node.angles,
        "scales": w.node_scales.get(&id).copied().unwrap_or([1.0; 3]),
        "keyValues": keyvalues,
        "keyValueCount": node.keyvalues.len(),
        "unsupported": [],
        "connections": connections,
        "children": children,
        "childrenTruncated": children_truncated,
        "source": "live",
    }))
}

/// m2.19: the children of `id` as the `entity.get` rows `[{nodeID, class}]` (node-list order) and
/// whether the [`MAX_CHILDREN`] cap cut the list. One source for `entity.get` and the tie/untie
/// arms, so the tree edge can only ever read one way.
fn child_rows(w: &HammerWorld, id: i64) -> (Value, bool) {
    let children: Vec<&MockNode> = w
        .nodes
        .iter()
        .filter(|n| n.parent == Some(id))
        .take(MAX_CHILDREN + 1)
        .collect();
    let truncated = children.len() > MAX_CHILDREN;
    let rows: Vec<Value> = children
        .iter()
        .take(MAX_CHILDREN)
        .map(|n| json!({"nodeID": n.node_id, "class": n.class}))
        .collect();
    (Value::Array(rows), truncated)
}

/// m2.19: `id`'s tied mesh children, ascending by nodeID (`entity.untie`'s list and the tie's
/// "already tied" check read the same edge). Only `class:"mesh"` nodes count: a tied entity child
/// would be a group membership, not a brush volume, and the tie only ever writes mesh children.
fn mesh_children(w: &HammerWorld, id: i64) -> Vec<i64> {
    let mut out: Vec<i64> = w
        .nodes
        .iter()
        .filter(|n| n.parent == Some(id) && n.class == "mesh")
        .map(|n| n.node_id)
        .collect();
    out.sort_unstable();
    out
}

/// `entity.find` (plan §3.2): entities only, case-insensitive exact class/targetname,
/// optional sphere on the origin, `limit` (default 500, at most 2000).
fn op_entity_find(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let cls = params.get("class").and_then(Value::as_str);
    let targetname = params.get("targetname").and_then(Value::as_str);
    // an empty filter is read by the bridge as *no filter* (it would answer the whole document):
    // refuse it like the fixed bridge does, so the mock cannot hide the case (brief item 13).
    if cls.is_some_and(str::is_empty) {
        return Err(invalid_params("class: an empty filter is not allowed"));
    }
    if targetname.is_some_and(str::is_empty) {
        return Err(invalid_params("targetname: an empty filter is not allowed"));
    }
    if params
        .get("class")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(invalid_params("class: a string is required"));
    }
    if params
        .get("targetname")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return Err(invalid_params("targetname: a string is required"));
    }
    let within = match params.get("within") {
        None => None,
        Some(wv) if wv.is_object() => {
            let origin = num3(wv, "origin");
            let Some(radius) = wv.get("radius").and_then(Value::as_f64) else {
                return Err(invalid_params("within: {origin:[3], radius:number} required"));
            };
            Some((origin, radius))
        }
        Some(_) => return Err(invalid_params("within: {origin:[3], radius:number} required")),
    };
    let mut limit = params.get("limit").and_then(Value::as_i64).unwrap_or(500);
    if limit > 2000 {
        limit = 2000;
    }
    if limit < 1 {
        limit = 500;
    }
    let visited = w.nodes.len();
    let mut entities: Vec<Value> = vec![];
    let mut truncated = false;
    for n in w.nodes.iter().filter(|n| n.class == "entity") {
        if let Some(c) = cls {
            if !n.classname.eq_ignore_ascii_case(c) {
                continue;
            }
        }
        if let Some(t) = targetname {
            match &n.targetname {
                Some(name) if name.eq_ignore_ascii_case(t) => {}
                _ => continue,
            }
        }
        if let Some((origin, radius)) = within {
            let dist = n
                .origin
                .iter()
                .zip(origin)
                .map(|(a, b)| (a - b) * (a - b))
                .sum::<f64>()
                .sqrt();
            if dist > radius {
                continue;
            }
        }
        if entities.len() as i64 >= limit {
            truncated = true;
            break;
        }
        entities.push(json!({
            "nodeID": n.node_id,
            "classname": n.classname,
            "targetname": n.targetname,
            "origin": n.origin,
        }));
    }
    Ok(json!({
        "entities": entities,
        "count": entities.len(),
        "visited": visited,
        "truncated": truncated,
        "source": "live",
    }))
}

/// `node.list` (read-only walk): nodeID/class/origin rows, mesh rows carry their slot counts.
fn op_node_list(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let cls = params.get("class").and_then(Value::as_str);
    let mut limit = params.get("limit").and_then(Value::as_i64).unwrap_or(2000);
    if !(1..=10000).contains(&limit) {
        limit = 2000;
    }
    let visited = w.nodes.len();
    let mut nodes: Vec<Value> = vec![];
    let mut truncated = false;
    // the bridge walks the world's children: the world node itself is never listed
    for n in w.nodes.iter().filter(|n| n.class != "world") {
        if let Some(c) = cls {
            if c != "all" && c != n.class {
                continue;
            }
        }
        if nodes.len() as i64 >= limit {
            truncated = true;
            break;
        }
        let mut row = json!({"nodeID": n.node_id, "class": n.class, "origin": n.origin});
        if n.class == "mesh" {
            row["vertexSlots"] = json!(0);
            row["faceSlots"] = json!(0);
        }
        nodes.push(row);
    }
    Ok(json!({
        "nodes": nodes,
        "count": nodes.len(),
        "visited": visited,
        "truncated": truncated,
        "source": "live",
    }))
}

/// `node.delete {nodeIDs}` (`op_node_delete`): every id is validated before anything is
/// deleted (world -> `-32602`, missing -> `-32022`, locked -> `-32018`); one undo group
/// for the whole request.
fn op_node_delete(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let Some(ids) = params.get("nodeIDs").and_then(Value::as_array) else {
        return Err(invalid_params("nodeIDs: [int,...] required"));
    };
    if ids.is_empty() {
        return Err(invalid_params("nodeIDs: empty"));
    }
    if ids.len() > 256 {
        return Err(invalid_params("nodeIDs: up to 256 non-negative ints"));
    }
    let mut numeric: Vec<i64> = vec![];
    for v in ids {
        let Some(id) = v.as_i64().filter(|i| *i >= 0) else {
            return Err(invalid_params("nodeIDs: up to 256 non-negative ints"));
        };
        if !node_id_ok(id) {
            return Err(invalid_params(format!(
                "nodeID {id} is above the bridge's int range"
            )));
        }
        numeric.push(id);
    }
    for id in &numeric {
        let Some(n) = node_of(w, *id) else {
            return Err(node_not_found(*id));
        };
        if n.class == "world" || n.class == "other" {
            return Err(invalid_params(format!(
                "nodeID {} cannot be deleted ({})",
                id, n.class
            )));
        }
        if n.locked {
            return Err(err(
                -32018,
                "DocumentNotEditable",
                json!({"reason": format!("nodeID {id} is locked")}),
            ));
        }
    }
    let snapshot = w.snapshot();
    w.nodes.retain(|n| !numeric.contains(&n.node_id));
    // m2.19: a deleted node may be a brush entity's parent — the mock keeps the tree edge
    // pointing at a LIVE node (a dangling parent would make `children_of` claim children of a
    // node that is gone). The children stay in the document as world nodes, like a real delete
    // of the entity node outside the tie path does not take the meshes with it.
    // [I] whether Hammer's own `DeleteNode(entity)` deletes the TIED meshes with the entity is
    // unverified (re-triggers.md covers the other direction, node.delete of the last tied mesh);
    // the mock re-parents them to the world, which is review A9's flagged shape. Test live once and
    // align this if Hammer deletes them too.
    for node in w.nodes.iter_mut() {
        if node.parent.is_some_and(|parent| numeric.contains(&parent)) {
            node.parent = None;
        }
    }
    for id in &numeric {
        w.meshes.remove(id);
        // m2.18: a deleted node takes its connections and object-properties scales with it
        w.connections.remove(id);
        w.node_scales.remove(id);
    }
    w.push_undo("MCP: delete nodes", Some(snapshot));
    changed_epoch(w);
    Ok(json!({
        "deleted": numeric,
        "undoLabel": "MCP: delete nodes",
    }))
}

// ---------------------------------------------------------------------------
// m2.18 prop writes (node.setTransform / entity.addConnections / entity.removeConnections,
// `bridge/protocol-diff-m2.18.md` — the wire contract of target/re/BRIDGE-M2.18-SPEC.md §3/§4)
// ---------------------------------------------------------------------------

/// `node.setTransform`'s origin bound (the bridge refuses |coordinate| > 1048576 before its undo
/// group opens).
const TRANSFORM_ORIGIN_MAX_ABS: f64 = 1_048_576.0;
/// `node.setTransform`'s / `entity.create`'s scales bound (0 exclusive, this inclusive).
const TRANSFORM_SCALE_MAX: f64 = 1000.0;
/// One `entity.addConnections` request carries at most this many connections (§4).
const MAX_CONNECTIONS: usize = 64;
/// `entity.removeConnections`' `indices` cap: the bridge's `RM_INDEX_MAX` (512, what `entity.get`
/// lists and one call may remove).
const MAX_INDICES: usize = 512;
/// `output`/`target`/`input`/`param` are at most this many bytes (§4).
const MAX_CONN_BYTES: usize = 255;
/// `delay` is finite in `0..=`[`MAX_CONN_DELAY`] (§4).
const MAX_CONN_DELAY: f64 = 3600.0;

/// The `nodeID` gate the m2.18 entity writes share: an int in range, an existing node, an entity,
/// unlocked. Everything runs before the undo group opens, like the bridge's own gates.
fn io_entity_target(w: &HammerWorld, params: &Value) -> Result<i64, Value> {
    let Some(id) = params
        .get("nodeID")
        .and_then(Value::as_i64)
        .filter(|id| node_id_ok(*id))
    else {
        return Err(invalid_params(
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    let Some(node) = node_of(w, id) else {
        return Err(node_not_found(id));
    };
    if node.class != "entity" {
        return Err(invalid_params("not an entity"));
    }
    if node.locked {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": format!("nodeID {id} is locked")}),
        ));
    }
    Ok(id)
}

/// `[x, y, z]` of finite numbers, or `None` when the member is absent/null. Shape only: the range
/// gates are spelled out per member (origin 1048576, scales (0, 1000]) where the spec has them.
fn number3_at(params: &Value, key: &str) -> Result<Option<[f64; 3]>, Value> {
    let Some(value) = params.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(axes) = value.as_array().filter(|axes| axes.len() == 3) else {
        return Err(invalid_params(format!("{key}: [x,y,z] numbers required")));
    };
    let mut out = [0.0f64; 3];
    for (axis, value) in axes.iter().enumerate() {
        out[axis] = value
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| invalid_params(format!("{key}[{axis}]: a finite number is required")))?;
    }
    Ok(Some(out))
}

/// `output`/`target`/`input` (required, non-empty) and `param` (optional, may be empty): at most
/// [`MAX_CONN_BYTES`] bytes, no control characters (§4).
fn conn_string(entry: &Map<String, Value>, key: &str, required: bool) -> Result<String, Value> {
    let value = entry.get(key).filter(|value| !value.is_null());
    let Some(value) = value else {
        if required {
            return Err(invalid_params(format!("{key}: a non-empty string is required")));
        }
        return Ok(String::new());
    };
    let Some(text) = value.as_str() else {
        return Err(invalid_params(format!("{key}: a string is required")));
    };
    if required && text.is_empty() {
        return Err(invalid_params(format!("{key}: a non-empty string is required")));
    }
    if text.len() > MAX_CONN_BYTES {
        return Err(invalid_params(format!(
            "{key}: at most {MAX_CONN_BYTES} bytes"
        )));
    }
    if text.chars().any(char::is_control) {
        return Err(invalid_params(format!("{key}: no control characters")));
    }
    Ok(text.to_string())
}

/// One `entity.addConnections` entry -> the stored connection, or the `-32602` the bridge answers
/// before its undo group opens. `param` defaults to `""`, `delay` to `0`, `timesToFire` to `-1`
/// (the UI's defaults, §4 / `re-props.md` Q3).
fn parse_connection(entry: &Value) -> Result<MockConnection, Value> {
    let Some(object) = entry.as_object() else {
        return Err(invalid_params("connections: each entry must be an object"));
    };
    let output = conn_string(object, "output", true)?;
    let target = conn_string(object, "target", true)?;
    let input = conn_string(object, "input", true)?;
    let param = conn_string(object, "param", false)?;
    let delay = match object.get("delay").filter(|value| !value.is_null()) {
        None => 0.0,
        Some(value) => value
            .as_f64()
            .filter(|delay| delay.is_finite() && (0.0..=MAX_CONN_DELAY).contains(delay))
            .ok_or_else(|| invalid_params(format!("delay: a number in 0..={MAX_CONN_DELAY}")))?,
    };
    let times_to_fire = match object.get("timesToFire").filter(|value| !value.is_null()) {
        None => -1,
        Some(value) => value
            .as_i64()
            .filter(|times| *times >= -1 && *times != 0)
            .ok_or_else(|| invalid_params("timesToFire: an int >= -1 (0 is not a valid count)"))?,
    };
    Ok(MockConnection {
        output,
        target,
        input,
        param,
        delay,
        times_to_fire,
    })
}

/// One connection as the tool-facing `added` entry: the bridge's own field names (`sb_add_conn_fields`,
/// the same ones `entity.get` and the `.vmap` reader use), `index` in the live vector. Review W2 #2:
/// the mock used to emit `output/target/input/param`, a shape no real reply ever has.
fn connection_json(index: i64, c: &MockConnection) -> Value {
    json!({
        "index": index,
        "outputName": c.output,
        "targetType": 7,
        "targetName": c.target,
        "inputName": c.input,
        "overrideParam": c.param,
        "delay": c.delay,
        "timesToFire": c.times_to_fire,
    })
}

/// `node.setTransform {nodeID, origin?, angles?, scales?, expectDoc}` (spec §3): the class gate
/// (entity or mesh), at least one member, the member bounds, all before the undo group opens. The
/// reply reads the node's fields back.
fn op_set_transform(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let Some(id) = params
        .get("nodeID")
        .and_then(Value::as_i64)
        .filter(|id| node_id_ok(*id))
    else {
        return Err(invalid_params(
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    let Some(node) = node_of(w, id) else {
        return Err(node_not_found(id));
    };
    if node.class != "entity" && node.class != "mesh" {
        return Err(invalid_params("node is not an entity or a mesh"));
    }
    if node.locked {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": format!("nodeID {id} is locked")}),
        ));
    }
    let origin = number3_at(params, "origin")?;
    let angles = number3_at(params, "angles")?;
    let scales = number3_at(params, "scales")?;
    if origin.is_none() && angles.is_none() && scales.is_none() {
        return Err(invalid_params(
            "at least one of origin, angles or scales is required",
        ));
    }
    if let Some(origin) = &origin {
        if origin.iter().any(|axis| axis.abs() > TRANSFORM_ORIGIN_MAX_ABS) {
            return Err(invalid_params(format!(
                "origin: each coordinate within +-{TRANSFORM_ORIGIN_MAX_ABS}"
            )));
        }
    }
    if let Some(scales) = &scales {
        if scales
            .iter()
            .any(|axis| *axis <= 0.0 || *axis > TRANSFORM_SCALE_MAX)
        {
            return Err(invalid_params(format!(
                "scales: each in (0, {TRANSFORM_SCALE_MAX}]"
            )));
        }
    }
    let snapshot = w.snapshot();
    let class = node.class.clone();
    if let Some(node) = w.nodes.iter_mut().find(|node| node.node_id == id) {
        if let Some(origin) = origin {
            node.origin = origin;
        }
        if let Some(angles) = angles {
            node.angles = angles;
        }
    }
    if let Some(scales) = scales {
        w.node_scales.insert(id, scales);
    }
    w.push_undo("MCP: set transform", Some(snapshot));
    changed_epoch(w);
    let node = node_of(w, id).expect("the node was there a moment ago");
    let scales = w.node_scales.get(&id).copied().unwrap_or([1.0; 3]);
    Ok(json!({
        "nodeID": id,
        "class": class,
        "origin": node.origin,
        "angles": node.angles,
        "scales": scales,
    }))
}

/// `entity.addConnections {nodeID, connections:[{output, target, input, param?, delay?,
/// timesToFire?}], expectDoc}` (spec §4): every connection is validated before the undo group
/// opens, then appended in list order; `added[i].index` is the entry's index in the live vector.
fn op_add_connections(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = io_entity_target(w, params)?;
    let Some(list) = params.get("connections").and_then(Value::as_array) else {
        return Err(invalid_params("connections: a non-empty array is required"));
    };
    if list.is_empty() {
        return Err(invalid_params("connections: a non-empty array is required"));
    }
    if list.len() > MAX_CONNECTIONS {
        return Err(invalid_params(format!(
            "connections: at most {MAX_CONNECTIONS} per request"
        )));
    }
    // all of them before anything is written (the bridge validates before undo_open)
    let mut parsed: Vec<MockConnection> = Vec::with_capacity(list.len());
    for entry in list {
        parsed.push(parse_connection(entry)?);
    }
    let snapshot = w.snapshot();
    let mut added: Vec<Value> = Vec::with_capacity(parsed.len());
    let mut landed: Vec<Value> = Vec::with_capacity(parsed.len());
    for (i, connection) in parsed.iter().enumerate() {
        // the bridge's read-back cross-check: an entry the vector did not confirm is `readback:
        // false` and is out of `count` (the injected `drop_conn_readback` seam is that failure)
        let confirmed = !w.drop_conn_readback.is_some_and(|at| at == i as i64);
        let index = w.connections.get(&id).map(Vec::len).unwrap_or(0) as i64;
        if confirmed {
            added.push(connection_json(index, connection));
            landed.push(json!({"index": index, "landed": true}));
        } else {
            added.push(json!({"readback": false, "index": index}));
            landed.push(json!({"index": index, "landed": false}));
        }
        w.connections.entry(id).or_default().push(connection.clone());
    }
    w.push_undo("MCP: add connections", Some(snapshot));
    changed_epoch(w);
    // count == the entries the vector CONFIRMED (never the calls attempted) and landed[] is per
    // wire entry - both exactly as the bridge answers them
    let count = added
        .iter()
        .filter(|entry| entry.get("readback").is_none())
        .count();
    Ok(json!({"nodeID": id, "added": added, "count": count, "landed": landed}))
}

/// One `match` member compared against a stored connection: the FGD/UI names are text, matched
/// case-insensitively (the exact comparison the live bridge performs is unverified — `[I]` in
/// `bridge/protocol-diff-m2.18.md`, which the orchestrator's live run pins).
fn connection_matches(c: &MockConnection, m: &Map<String, Value>) -> bool {
    let eq = |field: &Value, value: &str| {
        field
            .as_str()
            .is_some_and(|text| text.eq_ignore_ascii_case(value))
    };
    m.iter().all(|(key, wanted)| match key.as_str() {
        "output" => eq(wanted, &c.output),
        "target" => eq(wanted, &c.target),
        "input" => eq(wanted, &c.input),
        _ => false,
    })
}

/// `entity.removeConnections {nodeID, indices? | match?, expectDoc}` (spec §4): exactly one form,
/// the indices resolved against the live vector first (out of range / duplicate -> `-32602`, a
/// `match` with no hit -> `-32026 ConnectionNotFound`, the m2.18 bridge's code), then the entries removed in DESCENDING
/// order like the bridge. The reply reports the removed indices ascending and the count left.
fn op_remove_connections(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = io_entity_target(w, params)?;
    let indices = params.get("indices").filter(|value| !value.is_null());
    let matcher = params.get("match").filter(|value| !value.is_null());
    if indices.is_some() == matcher.is_some() {
        return Err(invalid_params(
            "exactly one of indices or match is required",
        ));
    }
    let count = w.connections.get(&id).map(Vec::len).unwrap_or(0);
    let mut resolved: Vec<usize> = Vec::new();
    if let Some(indices) = indices {
        let Some(list) = indices.as_array().filter(|list| !list.is_empty()) else {
            return Err(invalid_params("indices: a non-empty array of ints is required"));
        };
        if list.len() > MAX_INDICES {
            return Err(invalid_params(format!(
                "indices: at most {MAX_INDICES} per request"
            )));
        }
        for value in list {
            let Some(index) = value.as_u64().map(|index| index as usize) else {
                return Err(invalid_params("indices: non-negative ints required"));
            };
            if index >= count {
                return Err(invalid_params(format!(
                    "indices: {index} is out of range (the entity has {count} connection(s))"
                )));
            }
            if resolved.contains(&index) {
                return Err(invalid_params(format!("indices: {index} is listed twice")));
            }
            resolved.push(index);
        }
    } else {
        let Some(matcher) = matcher.and_then(Value::as_object) else {
            return Err(invalid_params(
                "match: an object with output/target/input is required",
            ));
        };
        if matcher.is_empty() {
            return Err(invalid_params(
                "match: at least one of output, target or input is required",
            ));
        }
        let mut wanted = Map::new();
        for (key, value) in matcher {
            if !matches!(key.as_str(), "output" | "target" | "input") {
                return Err(invalid_params(format!("match: unknown member {key:?}")));
            }
            let text = value
                .as_str()
                .filter(|text| !text.is_empty())
                .filter(|text| text.len() <= MAX_CONN_BYTES)
                .filter(|text| !text.chars().any(char::is_control))
                .ok_or_else(|| {
                    invalid_params(format!(
                        "match.{key}: a non-empty string of at most {MAX_CONN_BYTES} bytes without \
                         control characters is required"
                    ))
                })?;
            wanted.insert(key.clone(), json!(text));
        }
        let live = w.connections.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        resolved = (0..live.len())
            .filter(|index| connection_matches(&live[*index], &wanted))
            .collect();
        if resolved.is_empty() {
            return Err(err(
                -32026,
                "ConnectionNotFound",
                json!({"reason": "no live connection matches the match filter"}),
            ));
        }
    }
    let snapshot = w.snapshot();
    // descending order: `RemoveConnection` shifts the entries after the removed one
    let mut descending = resolved.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    for index in &descending {
        if let Some(vector) = w.connections.get_mut(&id) {
            if *index < vector.len() {
                vector.remove(*index);
            }
        }
    }
    let mut removed = resolved.clone();
    removed.sort_unstable();
    let remaining = w.connections.get(&id).map(Vec::len).unwrap_or(0);
    w.push_undo("MCP: remove connections", Some(snapshot));
    changed_epoch(w);
    // count == the number REMOVED (the bridge's own meaning, kept for compatibility); remaining ==
    // the vector size after the group (review W2 #1: the mock used to put the remainder in `count`,
    // so the Rust tool's tests proved the opposite of the bridge's semantics)
    Ok(json!({"nodeID": id, "removed": removed, "count": removed.len(),
              "remaining": remaining}))
}

// ---------------------------------------------------------------------------
// m2.19 brush-entity tie/untie (entity.tieMeshes / entity.untie, WP-T)
// ---------------------------------------------------------------------------

/// One `entity.tieMeshes` request ties at most this many meshes (the m2.19 spec's `1..64`).
const MAX_TIE_MESHES: usize = 64;
/// `entity.tieMeshes`' `class` is at most this many bytes (the m2.19 spec).
const MAX_CLASS_BYTES: usize = 63;
/// `entity.get` lists at most this many children (`childrenTruncated` beyond it; m2.19 spec).
const MAX_CHILDREN: usize = 1024;

/// `entity.tieMeshes` (m2.19 FINAL contract, `target/plan-triggers.md` §5): exactly one of
/// `class` (create a new brush entity) and `entityID` (tie into an existing one); `keyValues` only
/// with `class`; `allowRetie` (default false) lets a mesh that already belongs to ANOTHER brush
/// entity move, deleting the emptied old entity in the same group. A mesh already under the
/// target entity is skipped and reported in `alreadyTied`. Every gate runs **before** the undo
/// group opens (the review-13 rule; a refused tie leaves nothing behind), then one
/// `"MCP: tie meshes to entity"` group creates/uses the entity, applies `keyValues` through the
/// m2.18 `SetKeyValue` path ([`apply_keyvalues`]) and re-parents the meshes.
///
/// The reply's `origin` is the FIRST mesh's origin for a new entity (Hammer's own rule, RE Q1
/// `[V live]`) and the existing entity's own origin for `entityID`; it carries `created`,
/// `meshes [{nodeID, origin}]`, `alreadyTied`, `removedEmptyEntities` and the group's own label.
fn op_tie_meshes(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let class_value = params.get("class").filter(|value| !value.is_null());
    let entity_value = params.get("entityID").filter(|value| !value.is_null());
    // exactly one of class / entityID (§5)
    let existing: Option<i64> = match (class_value, entity_value) {
        (Some(_), Some(_)) => {
            return Err(invalid_params(
                "class and entityID are mutually exclusive: exactly one is required",
            ))
        }
        (None, None) => {
            return Err(invalid_params(
                "exactly one of class and entityID is required",
            ))
        }
        (None, Some(value)) => {
            Some(value.as_i64().filter(|id| node_id_ok(*id)).ok_or_else(|| {
                invalid_params("entityID: a non-negative int up to 2147483647 is required")
            })?)
        }
        (Some(_), None) => None,
    };

    // the class form: the string gates, the factory registry, then the solid-class check
    let class: Option<String> = match existing {
        Some(_) => None,
        None => {
            let Some(class) = params
                .get("class")
                .and_then(Value::as_str)
                .filter(|class| !class.is_empty())
            else {
                return Err(invalid_params("class: a non-empty class name is required"));
            };
            if class.len() > MAX_CLASS_BYTES {
                return Err(invalid_params(format!(
                    "class: at most {MAX_CLASS_BYTES} bytes"
                )));
            }
            if !ENTITY_CLASSES.contains(&class) {
                return Err(err(
                    -32006,
                    "UnknownClass",
                    json!({"reason": "not in the CMapNodeFactory registry"}),
                ));
            }
            if !SOLID_CLASSES.contains(&class) {
                // the bridge's defensive gdType != 5 check runs AFTER `CreateNodeByName`, i.e.
                // inside the undo group it then leaves behind (it deletes the created node in the
                // same group). The mock refuses before its own group opens, so `undoGroupOpened`
                // is the whole model of that group: nothing of the caller's is in the document and
                // the mock's undo stack is untouched. [I] the empty group itself is not modelled.
                return Err(err(
                    -32602,
                    "InvalidParams",
                    json!({"reason": "not a solid (brush) class", "undoGroupOpened": true}),
                ));
            }
            Some(class.to_string())
        }
    };
    // the entityID form: a live CMapEntity of a solid class, unlocked, with no keyValues (the
    // class form's `SetKeyValue` path does not apply to an entity that already exists)
    let target: Option<(i64, [f64; 3])> = match existing {
        None => None,
        Some(id) => {
            if params
                .get("keyValues")
                .is_some_and(|value| !value.is_null())
            {
                return Err(invalid_params(
                    "keyValues: only allowed with class (an existing entity keeps its own)",
                ));
            }
            let Some(node) = node_of(w, id) else {
                return Err(node_not_found(id));
            };
            if node.class != "entity" || !SOLID_CLASSES.contains(&node.classname.as_str()) {
                return Err(invalid_params(format!(
                    "entityID {id} is not a brush entity"
                )));
            }
            if node.locked {
                return Err(err(
                    -32018,
                    "DocumentNotEditable",
                    json!({"reason": format!("nodeID {id} is locked")}),
                ));
            }
            Some((id, node.origin))
        }
    };

    let Some(ids) = params.get("nodeIDs").and_then(Value::as_array) else {
        return Err(invalid_params(format!(
            "nodeIDs: an array of 1..={MAX_TIE_MESHES} ints is required"
        )));
    };
    if ids.is_empty() {
        return Err(invalid_params(format!(
            "nodeIDs: an array of 1..={MAX_TIE_MESHES} ints is required"
        )));
    }
    if ids.len() > MAX_TIE_MESHES {
        return Err(invalid_params(format!(
            "nodeIDs: at most {MAX_TIE_MESHES} per request"
        )));
    }
    let mut numeric: Vec<i64> = Vec::with_capacity(ids.len());
    for value in ids {
        let Some(id) = value.as_i64().filter(|id| node_id_ok(*id)) else {
            return Err(invalid_params(
                "nodeIDs: non-negative ints up to 2147483647 are required",
            ));
        };
        numeric.push(id);
    }
    let allow_retie = params
        .get("allowRetie")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // every id before anything is created, in the spec's order: the lock is checked **before** the
    // already-tied skip (the bridge's own order, review A9/L11), a mesh under ANOTHER brush entity
    // is refused unless `allowRetie` moves it, a mesh already under the target is skipped. Every
    // refusal carries the node it is about (`tie_err_node`'s `{reason, nodeID}` shape); the
    // `entityID`'s own -32022 carries none, which is what lets the tool attribute it (A3).
    let mut already_tied: Vec<i64> = Vec::new();
    for id in &numeric {
        let Some(node) = node_of(w, *id) else {
            return Err(err(
                -32022,
                "NodeNotFound",
                json!({"reason": format!("nodeID {id} not found"), "nodeID": id}),
            ));
        };
        if node.class != "mesh" {
            return Err(err(
                -32602,
                "InvalidParams",
                json!({"reason": "not a mesh", "nodeID": id}),
            ));
        }
        if node.locked {
            return Err(err(
                -32018,
                "DocumentNotEditable",
                json!({"reason": format!("nodeID {id} is locked"), "nodeID": id}),
            ));
        }
        if let Some(parent) = node.parent {
            if target.is_some_and(|(id, _)| id == parent) {
                already_tied.push(*id);
                continue;
            }
            if !allow_retie {
                return Err(err(
                    -32602,
                    "InvalidParams",
                    json!({
                        "reason": format!("nodeID {id} is already tied to nodeID {parent}"),
                        "nodeID": id,
                        "parentNodeID": parent,
                    }),
                ));
            }
        }
    }

    // §5 / review-b L4: a tie whose every mesh is already under the target opens NO undo group —
    // a success no-op (`noop:true`, `undoLabel:null`), no `push_undo`, no epoch move. The Rust
    // tool reads the marker and reports `undoGroups:0` with the "nothing to do" note (A8).
    if let Some((id, origin)) = target {
        if already_tied.len() == numeric.len() {
            let classname = node_of(w, id)
                .map(|n| n.classname.clone())
                .unwrap_or_default();
            return Ok(json!({
                "nodeID": id,
                "class": "entity",
                "classname": classname,
                "created": false,
                "origin": origin,
                "meshes": mesh_rows(w, &numeric),
                "alreadyTied": already_tied,
                "removedEmptyEntities": [],
                "noop": true,
                "undoLabel": Value::Null,
            }));
        }
    }

    let snapshot = w.snapshot();
    let mut keyvalues_echo: Option<Value> = None;
    let mut handle: Option<String> = None;
    // create the entity (class form) or address the existing one, then re-parent
    let (entity, classname, created, origin) = match target {
        Some((id, origin)) => {
            let classname = node_of(w, id)
                .map(|n| n.classname.clone())
                .unwrap_or_default();
            (id, classname, false, origin)
        }
        None => {
            // Hammer's rule: a new entity's origin/angles are the FIRST mesh's
            let first = node_of(w, numeric[0])
                .map(|n| (n.origin, n.angles))
                .unwrap_or(([0.0; 3], [0.0; 3]));
            let classname = class.expect("the class form checked the name");
            let id = next_node_id(w);
            let mut node = MockNode {
                node_id: id,
                class: "entity".into(),
                classname: classname.clone(),
                targetname: None,
                origin: first.0,
                angles: first.1,
                keyvalues: BTreeMap::new(),
                locked: false,
                parent: None,
            };
            // the m2.18 keyvalue path, after the create (the spec's `SetKeyValue` recipe); the
            // reply echoes what it wrote (A4: the only signal for a refused key)
            if let Some(keyvalues) = params.get("keyValues").filter(|value| value.is_object()) {
                let (applied, warnings, _) = apply_keyvalues(&mut node, keyvalues);
                keyvalues_echo = Some(json!({"applied": applied, "warnings": warnings}));
            }
            let node_origin = node.origin;
            w.nodes.push(node);
            // the handle the bridge issues for the node it created (batch-scoped, like
            // `entity.create`'s; the entityID form creates nothing and sends none)
            w.handle_seq += 1;
            handle = Some(format!("n{}", w.handle_seq));
            (id, classname, true, node_origin)
        }
    };
    // re-parent: `alreadyTied` ids stay, everything else moves under `entity`; an old brush entity
    // left without children is deleted in the same group (`removedEmptyEntities`)
    let mut removed_empty: Vec<i64> = Vec::new();
    for mesh in &numeric {
        let old = node_of(w, *mesh).and_then(|node| node.parent);
        if old == Some(entity) {
            continue;
        }
        if let Some(node) = w.nodes.iter_mut().find(|node| node.node_id == *mesh) {
            node.parent = Some(entity);
        }
        if let Some(old) = old {
            let still_has_children = w.nodes.iter().any(|node| node.parent == Some(old));
            if !still_has_children {
                removed_empty.push(old);
                w.nodes.retain(|node| node.node_id != old);
                // the deleted old entity takes its connections and scales with it (node.delete)
                w.connections.remove(&old);
                w.node_scales.remove(&old);
            }
        }
    }
    // the tied meshes as the reply reports them: nodeID + the node's own origin, request order
    let meshes = mesh_rows(w, &numeric);
    w.push_undo("MCP: tie meshes to entity", Some(snapshot));
    changed_epoch(w);
    let mut reply = json!({
        "nodeID": entity,
        "class": "entity",
        "classname": classname,
        "created": created,
        "origin": origin,
        "meshes": meshes,
        "alreadyTied": already_tied,
        "removedEmptyEntities": removed_empty,
        "undoLabel": "MCP: tie meshes to entity",
    });
    if let Some(handle) = handle {
        reply["handle"] = json!(handle);
    }
    // A4: the bridge echoes what `apply_keyvalues` wrote when it was given keyValues
    if let Some(echo) = keyvalues_echo {
        reply["keyValues"] = echo;
    }
    Ok(reply)
}

/// The tied meshes one tie reply reports: `[{nodeID, origin}]` in request order (the node's own
/// origin, not the entity's).
fn mesh_rows(w: &HammerWorld, ids: &[i64]) -> Vec<Value> {
    ids.iter()
        .filter_map(|mesh| {
            node_of(w, *mesh).map(|node| json!({"nodeID": node.node_id, "origin": node.origin}))
        })
        .collect()
}

/// `entity.untie {nodeID, expectDoc}` (m2.19 FINAL contract, §5): the reverse of the tie — the
/// entity's tied meshes go back under the entity's parent and the entity node is deleted (with
/// its connections and object-properties scales), one `"Untie from entity"` group (Hammer's own
/// `MoveToWorld` label). A node with no mesh children is `-32602 "not a brush entity"`, a
/// non-entity `-32602 "not an entity"`, both before the group opens.
///
/// The reply's `meshes` is the mesh-only, ascending list ([`mesh_children`]); the bridge returns
/// every map-node child in child-handle order (review A9/L11). The Rust tool sorts what it reads,
/// so the difference is invisible to callers — noted here rather than modelled.
fn op_untie(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let Some(id) = params
        .get("nodeID")
        .and_then(Value::as_i64)
        .filter(|id| node_id_ok(*id))
    else {
        return Err(invalid_params(
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    let Some(node) = node_of(w, id) else {
        return Err(node_not_found(id));
    };
    if node.class != "entity" {
        return Err(invalid_params("not an entity"));
    }
    if node.locked {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": format!("nodeID {id} is locked")}),
        ));
    }
    let meshes = mesh_children(w, id);
    if meshes.is_empty() {
        return Err(invalid_params(
            "not a brush entity: the node has no mesh children",
        ));
    }
    let snapshot = w.snapshot();
    for mesh in &meshes {
        if let Some(node) = w.nodes.iter_mut().find(|node| node.node_id == *mesh) {
            node.parent = None;
        }
    }
    w.nodes.retain(|node| node.node_id != id);
    // the deleted entity takes its connections and object-properties scales with it (node.delete)
    w.connections.remove(&id);
    w.node_scales.remove(&id);
    w.push_undo("Untie from entity", Some(snapshot));
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "meshes": meshes,
        "entityDeleted": true,
        "undoLabel": "Untie from entity",
    }))
}

// ---------------------------------------------------------------------------
// geometry (mesh.create / mesh.setGeometry / mesh.setMaterial)
// ---------------------------------------------------------------------------

/// Vertices/faces/material bounds of `geom_parse` (bridge `GEO_MAX_*`).
const MAX_VERTS: usize = 65536;
const MAX_FACES: usize = 65536;
const MAX_MATS: usize = 64;
/// The bridge's material buffer (`char mats[64][260]`, the NUL included).
const MAX_MATERIAL_BYTES: usize = 259;

/// One parsed geometry request, the way `geom_parse` reads it.
struct MockGeom {
    vertices: Vec<[f64; 3]>,
    faces: Vec<Vec<usize>>,
    /// One material per face (a `null`/missing/empty entry is the base material).
    materials: Vec<String>,
    /// Distinct materials that are not the base (`materials` in the reply is this + 1).
    distinct: usize,
}

/// `geom_parse` (`bridge/mcp_bridge.c`) with its own refusal reasons. Like the bridge it reads the
/// numbers itself: an integer index must be an integer, a coordinate must be strictly inside
/// +-1e6, and a material is a non-empty string.
fn parse_geom(params: &Value) -> Result<MockGeom, Value> {
    let Some(vertices) = params.get("vertices").and_then(Value::as_array) else {
        return Err(invalid_params("vertices: [[x,y,z],...] required"));
    };
    if vertices.len() < 3 {
        return Err(invalid_params("vertices: at least 3 [x,y,z]"));
    }
    if vertices.len() > MAX_VERTS {
        return Err(invalid_params("too many vertices (max 65536)"));
    }
    let mut vs: Vec<[f64; 3]> = Vec::with_capacity(vertices.len());
    for vertex in vertices {
        let point = vertex.as_array().filter(|axes| axes.len() == 3).and_then(|axes| {
            let mut out = [0.0f64; 3];
            for (axis, value) in axes.iter().enumerate() {
                out[axis] = value.as_f64().filter(|n| n.is_finite())?;
            }
            Some(out)
        });
        let Some(point) = point else {
            return Err(invalid_params(
                "vertices: each must be [x,y,z] with |coord| < 1e6",
            ));
        };
        if point.iter().any(|c| !(*c > -1e6 && *c < 1e6)) {
            return Err(invalid_params(
                "vertices: each must be [x,y,z] with |coord| < 1e6",
            ));
        }
        vs.push(point);
    }

    let Some(faces) = params.get("faces").and_then(Value::as_array) else {
        return Err(invalid_params("faces: [[i,j,k,...],...] required"));
    };
    if faces.is_empty() {
        return Err(invalid_params("faces: at least one face"));
    }
    if faces.len() > MAX_FACES {
        return Err(invalid_params("too many faces (max 65536)"));
    }
    let mut fs: Vec<Vec<usize>> = Vec::with_capacity(faces.len());
    for face in faces {
        let Some(loop_) = face.as_array().filter(|loop_| loop_.len() >= 3) else {
            return Err(invalid_params("faces: each needs at least 3 vertices"));
        };
        let mut out: Vec<usize> = Vec::with_capacity(loop_.len());
        for index in loop_ {
            let Some(index) = index
                .as_u64()
                .map(|index| index as usize)
                .filter(|index| *index < vs.len())
            else {
                return Err(invalid_params(
                    "faces: vertex indices must be integers in range",
                ));
            };
            out.push(index);
        }
        fs.push(out);
    }

    let Some(base) = params
        .get("material")
        .and_then(Value::as_str)
        .filter(|material| !material.is_empty())
    else {
        return Err(invalid_params(
            "material: a material path, e.g. materials/dev/reflectivity_30.vmat",
        ));
    };
    let base = base.to_string();
    if base.len() > MAX_MATERIAL_BYTES || base.chars().any(char::is_control) {
        return Err(invalid_params(
            "material: a material path, e.g. materials/dev/reflectivity_30.vmat",
        ));
    }

    let mut materials = vec![base.clone(); fs.len()];
    let mut extra: Vec<String> = Vec::new();
    if let Some(list) = params.get("faceMaterials").filter(|list| !list.is_null()) {
        let Some(list) = list.as_array() else {
            return Err(invalid_params("faceMaterials: strings or null"));
        };
        if list.len() > fs.len() {
            return Err(invalid_params("faceMaterials: longer than faces"));
        }
        for (index, material) in list.iter().enumerate() {
            match material {
                // the bridge skips `null` and the empty string (both mean "the default")
                Value::Null => {}
                Value::String(text) if text.is_empty() => {}
                Value::String(text) => {
                    if text.len() > MAX_MATERIAL_BYTES || text.chars().any(char::is_control) {
                        return Err(invalid_params("faceMaterials: strings or null"));
                    }
                    if text != &base && !extra.iter().any(|seen| seen == text) {
                        if extra.len() >= MAX_MATS {
                            return Err(invalid_params(
                                "faceMaterials: at most 64 distinct materials",
                            ));
                        }
                        extra.push(text.clone());
                    }
                    materials[index] = text.clone();
                }
                _ => return Err(invalid_params("faceMaterials: strings or null")),
            }
        }
    }
    Ok(MockGeom {
        vertices: vs,
        faces: fs,
        materials,
        distinct: extra.len(),
    })
}

/// The pivot re-centre both create and setGeometry do: the geometry's bounds centre, rounded to 3
/// decimals (the POC mock's `centre`).
fn geom_centre(vertices: &[[f64; 3]]) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    for (axis, slot) in out.iter_mut().enumerate() {
        let mut mn = f64::INFINITY;
        let mut mx = f64::NEG_INFINITY;
        for point in vertices {
            mn = mn.min(point[axis]);
            mx = mx.max(point[axis]);
        }
        *slot = ((mn + mx) / 2.0 * 1000.0).round() / 1000.0;
    }
    out
}

/// `geo_target_mesh`: the nodeID gate and the mesh-class check, both before the undo group opens
/// (so a missing/wrong-class node writes nothing).
fn geom_target(w: &HammerWorld, params: &Value) -> Result<i64, Value> {
    let id = params
        .get("nodeID")
        .and_then(Value::as_i64)
        .filter(|id| node_id_ok(*id))
        .ok_or_else(|| invalid_params("nodeID: a non-negative int up to 2147483647 is required"))?;
    let node = node_of(w, id).ok_or_else(|| node_not_found(id))?;
    if node.class != "mesh" {
        return Err(invalid_params("node is not a mesh"));
    }
    Ok(id)
}

/// `op_mesh_create`: a new CMapMesh node holding the parsed geometry, pivot on the bounds centre.
fn op_mesh_create(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let mut geom = parse_geom(params)?;
    w.weld_drop(&mut geom);
    let nv = geom.vertices.len();
    let nf = geom.faces.len();
    let materials = geom.distinct + 1;
    let origin = geom_centre(&geom.vertices);
    let id = next_node_id(w);
    w.nodes.push(MockNode {
        node_id: id,
        class: "mesh".into(),
        classname: String::new(),
        targetname: None,
        origin,
        angles: [0.0; 3],
        keyvalues: BTreeMap::new(),
        locked: false,
        parent: None,
    });
    w.meshes.insert(
        id,
        MockMesh {
            vertices: geom.vertices,
            faces: geom.faces,
            materials: geom.materials,
        },
    );
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": origin,
        "vertexSlots": nv,
        "faceSlots": nf,
        "inputVertices": nv,
        "faces": nf,
        "materials": materials,
        "undoLabel": "MCP: create mesh",
    }))
}

/// `op_mesh_set_geometry`: clear the mesh and rebuild it from the request (the node keeps its id).
fn op_mesh_set_geometry(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = geom_target(w, params)?;
    let mut geom = parse_geom(params)?;
    w.weld_drop(&mut geom);
    let nv = geom.vertices.len();
    let nf = geom.faces.len();
    let origin = geom_centre(&geom.vertices);
    if let Some(node) = w.nodes.iter_mut().find(|node| node.node_id == id) {
        node.origin = origin;
    }
    w.meshes.insert(
        id,
        MockMesh {
            vertices: geom.vertices,
            faces: geom.faces,
            materials: geom.materials,
        },
    );
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": origin,
        "vertexSlots": nv,
        "faceSlots": nf,
        "inputVertices": nv,
        "faces": nf,
        "undoLabel": "MCP: set mesh geometry",
    }))
}

/// Source `AngleMatrix` (rows), the same convention as hammer-mcp's `vmap::mesh::angle_matrix`.
fn mock_angle_matrix(angles: [f64; 3]) -> [[f64; 3]; 3] {
    let (p, y, r) = (angles[0].to_radians(), angles[1].to_radians(), angles[2].to_radians());
    let (sp, cp, sy, cy, sr, cr) = (p.sin(), p.cos(), y.sin(), y.cos(), r.sin(), r.cos());
    [
        [cp * cy, sr * sp * cy - cr * sy, cr * sp * cy + sr * sy],
        [cp * sy, sr * sp * sy + cr * cy, cr * sp * sy - sr * cy],
        [-sp, sr * cp, cr * cp],
    ]
}

/// `op_mesh_get` (m3.12): the mesh's stored geometry in node-local space plus the transform that
/// maps it back (see [`HammerWorld::set_mesh_transform`]). Read-only: no undo group, no epoch
/// change. A mesh node with no stored geometry answers empty arrays. Materials are deduplicated in
/// first-use order, `faceMaterials` indexes them, like the bridge's reply.
fn op_mesh_get(w: &HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = geom_target(w, params)?;
    if w.mesh_too_large {
        let mesh = w.meshes.get(&id);
        return Err(err(
            -32602,
            "InvalidParams",
            json!({"reason": "mesh too large for one reply",
                   "vertexCount": mesh.map_or(0, |m| m.vertices.len()),
                   "faceCount": mesh.map_or(0, |m| m.faces.len()),
                   "maxLineBytes": 1_048_576}),
        ));
    }
    // the ancestors, nearest first, ending at the world (the bridge's `parentChain`); proven
    // world-space only through group/entity containers
    let mut parent_chain: Vec<String> = Vec::new();
    let mut world_space = true;
    let mut cursor = node_of(w, id).and_then(|node| node.parent);
    while let Some(parent_id) = cursor {
        let Some(parent) = node_of(w, parent_id) else { break };
        parent_chain.push(parent.class.clone());
        if parent.class != "group" && parent.class != "entity" && parent.class != "world" {
            world_space = false;
        }
        cursor = parent.parent;
        if parent_chain.len() > 64 {
            break;
        }
    }
    if parent_chain.last().map(String::as_str) != Some("world") {
        parent_chain.push("world".to_string());
    }
    let node_origin = node_of(w, id).map(|node| node.origin).unwrap_or([0.0; 3]);
    let xf = w.mesh_xf.get(&id).copied().unwrap_or(MeshTransform {
        origin: node_origin,
        angles: [0.0; 3],
        scales: [1.0; 3],
    });
    let matrix = mock_angle_matrix(xf.angles);
    let empty = MockMesh { vertices: Vec::new(), faces: Vec::new(), materials: Vec::new() };
    let mesh = w.meshes.get(&id).unwrap_or(&empty);
    // local = S^-1 * R^T * (world - origin)
    let vertices: Vec<Value> = mesh
        .vertices
        .iter()
        .map(|point| {
            let d = [point[0] - xf.origin[0], point[1] - xf.origin[1], point[2] - xf.origin[2]];
            let mut local = [0.0f64; 3];
            for (k, slot) in local.iter_mut().enumerate() {
                *slot = (matrix[0][k] * d[0] + matrix[1][k] * d[1] + matrix[2][k] * d[2]) / xf.scales[k];
            }
            json!(local)
        })
        .collect();
    let mut materials: Vec<String> = Vec::new();
    let mut face_materials: Vec<i64> = Vec::new();
    for face in 0..mesh.faces.len() {
        match mesh.materials.get(face) {
            Some(material) if !material.is_empty() => {
                let at = materials.iter().position(|m| m == material).unwrap_or_else(|| {
                    materials.push(material.clone());
                    materials.len() - 1
                });
                face_materials.push(at as i64);
            }
            _ => face_materials.push(-1),
        }
    }
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": xf.origin,
        "angles": xf.angles,
        "scales": xf.scales,
        "space": "local",
        "worldSpace": world_space,
        "parentChain": parent_chain,
        "vertices": vertices,
        "faces": mesh.faces,
        "faceMaterials": face_materials,
        "materials": materials,
        "vertexCount": mesh.vertices.len(),
        "faceCount": mesh.faces.len(),
    }))
}

/// `op_mesh_set_material`: every face of the mesh takes the material (the bridge collects all face
/// handles; a mesh with no faces is the bridge's `-32603 "mesh has no faces"`).
fn op_mesh_set_material(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = geom_target(w, params)?;
    let Some(material) = params
        .get("material")
        .and_then(Value::as_str)
        .filter(|material| !material.is_empty())
    else {
        return Err(invalid_params("material required"));
    };
    let material = material.to_string();
    let origin = node_of(w, id).map(|node| node.origin).unwrap_or([0.0; 3]);
    let Some(mesh) = w.meshes.get_mut(&id) else {
        return Err(err(
            -32603,
            "InternalError",
            json!({"reason": "mesh has no faces"}),
        ));
    };
    let count = mesh.faces.len();
    if count == 0 || count > MAX_FACES * 4 {
        return Err(err(
            -32603,
            "InternalError",
            json!({"reason": "mesh has no faces"}),
        ));
    }
    let vertex_slots = mesh.vertices.len();
    mesh.materials = vec![material.clone(); count];
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": origin,
        "vertexSlots": vertex_slots,
        "faceSlots": count,
        "faces": count,
        "material": material,
        "undoLabel": "MCP: set mesh material",
    }))
}

/// What a face's texture reads back as before anything touched it: U along +x, V along -y,
/// offsets 0, scale 0.25 world units per texel.
const DEFAULT_FACE_TEX: [f64; 10] = [1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.25, 0.25];

/// `mode "axes"` (m3.18): absolute per-face axes. The bridge's validation (finite numbers, scale > 0,
/// axes not zero-length and not parallel, unique in-range indices), the store, the read-back with
/// `exact:true` (the mock stores what it is given).
fn op_mesh_set_face_texture_axes(w: &mut HammerWorld, id: i64, params: &Value) -> Result<Value, Value> {
    let quad = |v: Option<&Value>, name: &str| -> Result<[f64; 4], Value> {
        let bad = || invalid_params(format!("{name}: [x, y, z, offset] of four finite numbers required"));
        let a = v.and_then(Value::as_array).filter(|a| a.len() == 4).ok_or_else(bad)?;
        let mut out = [0.0; 4];
        for (k, x) in a.iter().enumerate() {
            out[k] = x.as_f64().filter(|x| x.is_finite() && x.abs() < 1e30).ok_or_else(bad)?;
        }
        Ok(out)
    };
    let list = params
        .get("faces")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty())
        .ok_or_else(|| invalid_params("faces: an array of {index, axisU, axisV, scale} is required with mode \"axes\""))?;
    let Some(mesh) = w.meshes.get(&id) else {
        return Err(err(-32603, "InternalError", json!({"reason": "mesh has no faces"})));
    };
    let count = mesh.faces.len();
    let vertex_slots = mesh.vertices.len();
    let origin = node_of(w, id).map(|node| node.origin).unwrap_or([0.0; 3]);
    let mut writes: Vec<(usize, [f64; 10])> = Vec::new();
    for (k, entry) in list.iter().enumerate() {
        let index = entry
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_params(format!("faces[{k}].index: a non-negative integer is required")))? as usize;
        if index >= count {
            return Err(invalid_params(format!("faces[{k}]: index {index} out of range (the mesh has {count} faces)")));
        }
        if writes.iter().any(|(i, _)| *i == index) {
            return Err(invalid_params(format!("faces[{k}]: index {index} is listed twice")));
        }
        let u = quad(entry.get("axisU"), &format!("faces[{k}].axisU"))?;
        let v = quad(entry.get("axisV"), &format!("faces[{k}].axisV"))?;
        let s = entry
            .get("scale")
            .and_then(Value::as_array)
            .filter(|a| a.len() == 2)
            .and_then(|a| Some([a[0].as_f64()?, a[1].as_f64()?]))
            .filter(|s| s.iter().all(|x| x.is_finite() && *x > 0.0))
            .ok_or_else(|| invalid_params(format!("faces[{k}].scale: both components must be > 0 (world units per texel)")))?;
        let norm = |a: &[f64; 4]| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        if norm(&u) == 0.0 || norm(&v) == 0.0 {
            return Err(invalid_params(format!("faces[{k}].axisU: the xyz axis must not be zero-length")));
        }
        let cross = [u[1] * v[2] - u[2] * v[1], u[2] * v[0] - u[0] * v[2], u[0] * v[1] - u[1] * v[0]];
        if (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt() <= 1e-6 * norm(&u) * norm(&v) {
            return Err(invalid_params(format!("faces[{k}]: axisU and axisV must not be parallel")));
        }
        writes.push((index, [u[0], u[1], u[2], u[3], v[0], v[1], v[2], v[3], s[0], s[1]]));
    }
    let rows: Vec<Value> = writes
        .iter()
        .map(|(i, t)| json!({"index": i, "axisU": [t[0], t[1], t[2], t[3]], "axisV": [t[4], t[5], t[6], t[7]], "scale": [t[8], t[9]]}))
        .collect();
    let touched = writes.len();
    for (i, t) in writes {
        w.face_tex.insert((id, i), t);
    }
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": origin,
        "vertexSlots": vertex_slots,
        "faceSlots": count,
        "mode": "axes",
        "texSet": params.get("texSet").and_then(Value::as_u64).unwrap_or(0),
        "touched": touched,
        "exact": true,
        "readBackTruncated": false,
        "faces": rows,
        "undoLabel": "MCP: set face texture",
    }))
}

/// `op_mesh_set_face_texture` (m3.14): the bridge's validation order and reply shape over a mock
/// texture store. Modes only mimic the observable effect (world/face/justify reset the offsets,
/// world/face reset the scale unless `keepScale`), then shift/scale/rotate apply on top.
fn op_mesh_set_face_texture(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let id = geom_target(w, params)?;
    if w.face_texture_fail.as_deref() == Some("pre") {
        return Err(err(
            -32603,
            "TextureSetUnknown",
            json!({"reason": "face 0: the texture parameters of set 0 could not be read"}),
        ));
    }
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("");
    if mode == "axes" {
        return op_mesh_set_face_texture_axes(w, id, params);
    }
    if !["world", "face", "justify", "set"].contains(&mode) {
        return Err(invalid_params("mode: one of world, face, justify, set is required"));
    }
    if mode == "justify" && params.get("justify").and_then(Value::as_str).is_none() {
        return Err(invalid_params("justify: required when mode is \"justify\""));
    }
    if mode == "justify"
        && ["shift", "scale", "rotate"].iter().any(|k| params.get(*k).is_some_and(|v| !v.is_null()))
    {
        return Err(invalid_params(
            "shift, scale and rotate are not valid with mode \"justify\" (they would undo the justify)",
        ));
    }
    let pair = |name: &str| -> Result<Option<[f64; 2]>, Value> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let bad = || invalid_params(format!("{name}: [u, v] of two finite numbers required"));
                let a = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
                let x = a[0].as_f64().filter(|x| x.is_finite()).ok_or_else(bad)?;
                let y = a[1].as_f64().filter(|x| x.is_finite()).ok_or_else(bad)?;
                Ok(Some([x, y]))
            }
        }
    };
    let shift = pair("shift")?;
    let scale = pair("scale")?;
    if scale.is_some_and(|s| s[0] <= 0.0 || s[1] <= 0.0) {
        return Err(invalid_params("scale: both components must be > 0 (world units per texel)"));
    }
    let rotate = params.get("rotate").and_then(Value::as_f64);
    if mode == "set" && shift.is_none() && scale.is_none() && rotate.is_none() {
        return Err(invalid_params("mode \"set\" needs at least one of shift, scale, rotate"));
    }
    let keep_scale = params.get("keepScale").and_then(Value::as_bool).unwrap_or(false);
    let tex_set = params.get("texSet").and_then(Value::as_u64).unwrap_or(0);
    if tex_set > 1 {
        return Err(invalid_params("texSet: 0 or 1"));
    }
    let origin = node_of(w, id).map(|node| node.origin).unwrap_or([0.0; 3]);
    let Some(mesh) = w.meshes.get(&id) else {
        return Err(err(-32603, "InternalError", json!({"reason": "mesh has no faces"})));
    };
    let count = mesh.faces.len();
    let vertex_slots = mesh.vertices.len();
    if count == 0 {
        return Err(err(-32603, "InternalError", json!({"reason": "mesh has no faces"})));
    }
    let indices: Vec<usize> = match params.get("faces") {
        None | Some(Value::Null) => (0..count).collect(),
        Some(list) => {
            let Some(list) = list.as_array().filter(|l| !l.is_empty()) else {
                return Err(invalid_params("faces: an array of face indices is required"));
            };
            let mut out: Vec<usize> = Vec::new();
            for (k, v) in list.iter().enumerate() {
                let Some(i) = v.as_u64().map(|i| i as usize) else {
                    return Err(invalid_params(format!("faces[{k}]: a non-negative integer is required")));
                };
                if i >= count {
                    return Err(invalid_params(format!(
                        "faces[{k}]: index {i} out of range (the mesh has {count} faces)"
                    )));
                }
                if out.contains(&i) {
                    return Err(invalid_params(format!("faces[{k}]: index {i} is listed twice")));
                }
                out.push(i);
            }
            out
        }
    };
    let mut rows = Vec::new();
    for &i in &indices {
        let mut t = w.face_tex.get(&(id, i)).copied().unwrap_or(DEFAULT_FACE_TEX);
        if mode != "set" {
            t[3] = 0.0;
            t[7] = 0.0;
            if (mode == "world" || mode == "face") && !keep_scale {
                t[8] = 0.25;
                t[9] = 0.25;
            }
        }
        if let Some(deg) = rotate {
            let (s, c) = deg.to_radians().sin_cos();
            let (u, v) = ([t[0], t[1], t[2]], [t[4], t[5], t[6]]);
            for k in 0..3 {
                t[k] = c * u[k] + s * v[k];
                t[4 + k] = -s * u[k] + c * v[k];
            }
        }
        if let Some(sc) = scale {
            t[8] = sc[0];
            t[9] = sc[1];
        }
        if let Some(sh) = shift {
            t[3] = sh[0];
            t[7] = sh[1];
        }
        w.face_tex.insert((id, i), t);
        if rows.len() < 1000 {
            rows.push(json!({
                "index": i,
                "axisU": [t[0], t[1], t[2], t[3]],
                "axisV": [t[4], t[5], t[6], t[7]],
                "scale": [t[8], t[9]],
            }));
        }
    }
    changed_epoch(w);
    Ok(json!({
        "nodeID": id,
        "class": "mesh",
        "origin": origin,
        "vertexSlots": vertex_slots,
        "faceSlots": count,
        "mode": mode,
        "texSet": tex_set,
        "touched": indices.len(),
        "readBackTruncated": indices.len() > rows.len(),
        "faces": rows,
        "undoLabel": "MCP: set face texture",
    }))
}

/// `undo.undo`: pops the top group; a snapshot group restores the nodes, a keyvalue group pops
/// without restoring values (F2). An empty stack answers the bridge's `{"undone":false,"name":null}`.
///
/// The undo uses up one serial like live (`serial-probe.log`: create, create, undo, create reports
/// `topSerial` `1, 2, 1, 4` — `3` is never seen; review-4 R4-3), so the counter advances while the
/// reported top serial goes back to the new top group's own.
fn op_undo(w: &mut HammerWorld) -> Result<Value, Value> {
    let Some(entry) = w.undo.pop() else {
        return Ok(json!({"undone": false, "name": null}));
    };
    w.next_serial += 1;
    if let Some(snapshot) = entry.snapshot {
        w.nodes = snapshot.nodes;
        w.meshes = snapshot.meshes;
        w.connections = snapshot.connections;
        w.node_scales = snapshot.node_scales;
    }
    changed_epoch(w);
    Ok(json!({
        "undone": true,
        "name": entry.label,
        "undoDepth": w.undo.len(),
    }))
}

/// `map.save`: editable -> untitled gate (no `expectDoc`, like the bridge); a dirty document writes, a clean one
/// skips (`skippedClean`); either way the document answers clean afterwards.
fn op_map_save(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    if !w.editable {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": "not_editable"}),
        ));
    }
    // the real `map.save` arm checks editable and untitled, not `expectDoc` (mcp_bridge.c)
    let _ = params;
    if w.new_file || w.path.is_empty() {
        return Err(err(
            -32015,
            "UntitledDocument",
            json!({"reason": "no path (Save-As is modal)"}),
        ));
    }
    let skipped = !w.dirty;
    w.dirty = false;
    Ok(json!({"saved": true, "skippedClean": skipped, "path": w.path}))
}

/// `map.reload`: editable first (like the bridge arm), then `expectDoc` (the bridge's arm gained it
/// in m2.16: a document switch between `doc.active` and the reload is `-32004 DocumentChanged`, not
/// a forced reload of the other document - review L2), then the untitled gate, then `dirty && !force`
/// -> `-32016 UnsavedChanges {reason}`; with `force` (or a clean document) the reload from disk
/// marks the document clean again, drops the in-editor undo history (`undoDepth` -> 0, the live
/// test `bridge/test_reload.py` pins it) and bumps the fingerprint epoch. The mock keeps its
/// node list across a reload: `map.save` models the saved state only as the dirty flag, so
/// `discardedChanges` is the dirty flag *seen before* the reload.
fn op_map_reload(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    if !w.editable {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": "not_editable"}),
        ));
    }
    // the fingerprint gate every mutator has (FIX-3B L2): a tab switch between the caller's
    // `doc.active` and this reload is refused, so `force` can never discard *another* document's
    // edits. The bridge's `map.reload` arm checks it in the same place (after editable).
    expect_doc_ok(w, params)?;
    // the same untitled gate `map.save` has (FIX-3R M1, now also in the bridge arm): a new/untitled
    // document has no file to reload from. `force:true` does not unlock it.
    if w.new_file || w.path.is_empty() {
        return Err(err(
            -32015,
            "UntitledDocument",
            json!({"reason": "no path (Save-As is modal)"}),
        ));
    }
    let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
    if w.dirty && !force {
        return Err(err(
            -32016,
            "UnsavedChanges",
            json!({"reason": "document is dirty; pass force:true to discard"}),
        ));
    }
    let discarded = w.dirty;
    w.dirty = false;
    w.undo.clear();
    // the reload invalidates the fingerprint (the bridge calls `doc_fingerprint()` after
    // `ReloadFromDisk`); unlike the edit mutators it does *not* re-mark dirty
    w.epoch += 1;
    Ok(json!({
        "reloaded": true,
        "path": w.path,
        "discardedChanges": discarded,
        "epoch": w.epoch,
    }))
}

/// `…\csgo_addons\<addon>\<rest>` (a Windows document path, either separator) -> `<rest>` in lower
/// case with '/' separators: the addon-content-relative form `map.open` takes (lower case because
/// the comparison is case-insensitive, like the bridge's). `None` when the path has no
/// `csgo_addons\<addon>\` component or nothing after it.
pub(crate) fn addon_rel_of(win: &str) -> Option<String> {
    let lower = win.to_ascii_lowercase().replace('/', "\\");
    let at = lower.find("\\csgo_addons\\")?;
    let rest = &lower[at + "\\csgo_addons\\".len()..];
    let (_addon, tail) = rest.split_once('\\')?;
    if tail.is_empty() {
        return None;
    }
    Some(tail.replace('\\', "/"))
}

/// The `map.open` path-shape gate as `bridge/map_path.h` spells it, with its own reason strings
/// (`None` = accepted), in the bridge's own order (one rule per reason, so a mock test pins the
/// exact wire sentence). Review M1: the gate accepts the canonical addon-relative spelling only.
fn map_shape_reason(path: &str) -> Option<&'static str> {
    const EMPTY: &str =
        "path: a non-empty path relative to the addon content root (maps/... .vmap) is required";
    if path.is_empty() {
        return Some(EMPTY);
    }
    if path.len() > 512 {
        return Some("path: 1..512 chars");
    }
    if path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':') {
        return Some("path: relative to the addon content root (maps/...), not absolute");
    }
    // one pass over the bytes, exactly like the bridge's loop: the FIRST offending byte decides
    for b in path.bytes() {
        if b < 0x20 || b == 0x7f {
            return Some("path: no control characters");
        }
        if b == b':' {
            return Some("path: no ':'");
        }
        if b == b'\\' {
            return Some("path: no '\\' (use '/' separators)");
        }
    }
    let lower = path.to_ascii_lowercase();
    if !lower.ends_with(".vmap") {
        return Some("path: a .vmap file");
    }
    if !lower.starts_with("maps/") {
        return Some("path: under maps/ (the addon's map directory)");
    }
    if lower.len() <= "maps/".len() + ".vmap".len() {
        return Some("path: a map file name under maps/");
    }
    if lower.contains("..") {
        return Some("path: no '..'");
    }
    // review M1: the canonical segments (Win32 opens every other spelling for the same file)
    for segment in path["maps/".len()..].split('/') {
        if segment.is_empty() {
            return Some("path: no empty path segment ('//')");
        }
        if segment == "." || segment == ".." {
            return Some("path: no '.' or '..' path segment");
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Some("path: no path segment ending in '.' or a space");
        }
    }
    None
}

/// `map.open {path}` (m2.16): the shape gate, then the host gate, then the session-leak gate. A path
/// one of the sessions the editor holds already names answers `{opened:false, existing:true,
/// active:<is that session the active one>}` **without touching the document** — the bridge does not
/// call OpenFile there (the real editor's session match is a `xor al,al; ret` stub, so a second call
/// would create a SECOND session for the same file and leak the first; live, the app keeps the first
/// session in its vector, review M2). Anything else the host has opens: the mock's document becomes
/// that file, clean, with a new `id` and a bumped `epoch` (what `OpenFile` + `doc_fingerprint()` do
/// live), and its session is appended to the list.
fn op_map_open(w: &mut HammerWorld, params: &Value) -> Result<Value, Value> {
    let path = params.get("path").and_then(Value::as_str).unwrap_or("");
    if let Some(reason) = map_shape_reason(path) {
        return Err(invalid_params(reason));
    }
    let win = format!(
        "Z:\\app\\content\\csgo_addons\\{MOCK_ADDON}\\{}",
        path.replace('/', "\\")
    );
    if !w.map_files.iter().any(|f| f.eq_ignore_ascii_case(path)) {
        return Err(err(
            -32014,
            "AssetNotFound",
            json!({"reason": "no_such_file"}),
        ));
    }
    if let Some(held) = w.sessions.iter().find(|s| s.eq_ignore_ascii_case(&win)) {
        // a session already has this file: nothing opens, nothing changes (OpenFile is NOT called).
        // The ACTIVE session is the one the document currently belongs to (review M2: after
        // A -> B, opening A again is existing:true with active:false - the mock used to model a
        // single session and answered opened:true).
        let active = !w.new_file && w.path.eq_ignore_ascii_case(&win);
        let held = held.clone();
        return Ok(json!({
            "opened": false,
            "existing": true,
            "path": held,
            "active": active,
            "epoch": w.epoch,
        }));
    }
    w.doc_seq += 1;                 // a NEW session -> a new document identity
    w.epoch += 1;                   // ... and the fingerprint moves (doc_fingerprint)
    w.path = win.clone();
    w.new_file = false;
    w.dirty = false;
    w.undo.clear();                 // a fresh document has no undo history
    w.sessions.push(win.clone());
    if !w.map_files.iter().any(|f| f.eq_ignore_ascii_case(path)) {
        w.map_files.push(path.to_ascii_lowercase());
    }
    Ok(json!({
        "opened": true,
        "existing": false,
        "path": win,
        "epoch": w.epoch,
    }))
}

/// The engine handled a VToolMessage `open_asset` of the addon-relative `path`: the same state
/// change `map.open` makes (a new session, a new document), without the bridge method. Used by
/// [`crate::mock::MockBridge::hammer_open_asset`].
pub(crate) fn simulate_open_asset(w: &mut HammerWorld, path: &str) -> Result<Value, Value> {
    op_map_open(w, &json!({"path": path}))
}

/// The editable/expectDoc gate shared by every mutator (the geo/bespoke arm order: the
/// document is always present, so editable first, `expectDoc` second).
fn edit_gate(w: &mut HammerWorld, params: &Value) -> Result<(), Value> {
    if !w.editable {
        return Err(err(
            -32018,
            "DocumentNotEditable",
            json!({"reason": "not_editable"}),
        ));
    }
    expect_doc_ok(w, params)
}

/// The entity built-in dispatcher. Knobs sit in front of it (`dispatch` applies
/// `silent`/`drop`/`delay`/`error`/`responder` first), mirroring every other built-in.
/// Arms correspond 1:1 to `exec_request`'s; `geo_dispatch`'s order is doc -> editable ->
/// expectDoc -> op (an always-present document makes the first check moot here).
fn ok(v: Result<Value, Value>) -> Reply {
    match v {
        Ok(v) => (Some(v), None, false),
        Err(e) => (None, Some(e), false),
    }
}

type Reply = (Option<Value>, Option<Value>, bool);

pub fn builtin(st: &mut State, method: &str, params: &Value) -> Option<Reply> {
    let w = st.hammer.as_mut()?;
    match method {
        // m3.12: selection.* is only served when the world models it (`enable_selection`); else it
        // falls through to the unregistered-method answer, like a bridge without the rows.
        "selection.get" if w.selection.is_some() => Some(ok(selection::op_get(w))),
        "selection.set" if w.selection.is_some() => {
            // editable -> expectDoc -> op; no undo group, no dirty
            let r = if w.editable {
                selection::op_set(w, params)
            } else {
                Err(err(-32018, "DocumentNotEditable", json!({"reason": "not_editable"})))
            };
            Some(ok(r))
        }
        "doc.active" => {
            // review-2 L2 test seam: this read answers the document as-is; the identity switch
            // (a new `docSeq`/`epoch`, like opening a second session live) lands right *after*
            // it, so the next read answers the other document and the switch window opens
            let reply = Some((Some(json!({"doc": w.doc_json()})), None, false));
            w.doc_reads += 1;
            if w.switch_after_reads == Some(w.doc_reads) {
                w.doc_seq += 1;
                w.epoch += 1;
                w.switch_after_reads = None;
            }
            reply
        }
        "entity.get" => Some(ok(op_entity_get(w, params))),
        "entity.find" => Some(ok(op_entity_find(w, params))),
        "node.list" => Some(ok(op_node_list(w, params))),
        "entity.create" => {
            // the group is opened before the op (bridge order); the mock records it only on
            // success, since a failed op mutated nothing. The snapshot is the pre-create set.
            let snapshot = w.snapshot();
            let r = edit_gate(w, params).and_then(|_| op_create(w, params));
            if r.is_ok() {
                w.push_undo("MCP: create entity", Some(snapshot));
            }
            Some(ok(r.map(|(v, _)| with_doc(w, v))))
        }
        "entity.setKeyValues" => {
            // the group is opened before the op (bridge order); the mock records it only on
            // success, since a failed op mutated nothing. No handles live across requests,
            // so a standalone call's handle is always stale (`&[]`). m2.18: when the world
            // models `RecordNodeTree` (`set_value_undo_recorded`), the group carries the
            // pre-write snapshot, so popping it restores the values (the reply says "recorded").
            let snapshot = w.snapshot();
            let recorded = w.value_undo_recorded;
            let r = edit_gate(w, params).and_then(|_| op_set_keyvalues(w, params, &[]));
            if r.is_ok() {
                w.push_undo("MCP: set keyvalues", if recorded { Some(snapshot) } else { None });
            }
            Some(ok(r.map(|v| with_doc(w, v))))
        }
        "mesh.create" => {
            let snapshot = w.snapshot();
            let r = edit_gate(w, params).and_then(|_| op_mesh_create(w, params));
            if r.is_ok() {
                w.push_undo("MCP: create mesh", Some(snapshot));
                w.after_geometry_write();
            }
            Some(ok(r.map(|v| with_doc(w, v))))
        }
        "mesh.setGeometry" => {
            let snapshot = w.snapshot();
            let r = edit_gate(w, params).and_then(|_| op_mesh_set_geometry(w, params));
            if r.is_ok() {
                w.push_undo("MCP: set mesh geometry", Some(snapshot));
                w.after_geometry_write();
            }
            Some(ok(r.map(|v| with_doc(w, v))))
        }
        // m3.12: a read - no editability gate, no undo group; `expectDoc` is checked only when
        // the caller sends one (the bridge's `read_gate`).
        "mesh.get" if w.mesh_get => Some(ok(
            (if params.get("expectDoc").is_some() { expect_doc_ok(w, params) } else { Ok(()) })
                .and_then(|_| op_mesh_get(w, params))
                .map(|v| with_doc(w, v)),
        )),
        // m3.14: per-face texture alignment; same gate order as the other mesh writes.
        "mesh.setFaceTexture" if w.face_texture => {
            let snapshot = w.snapshot();
            let mut r = edit_gate(w, params).and_then(|_| op_mesh_set_face_texture(w, params));
            // The injected failures: "panic"/"mismatch" fail AFTER the write landed.
            let landed = r.is_ok()
                && matches!(w.face_texture_fail.as_deref(), Some("panic" | "mismatch"));
            if r.is_ok() {
                match w.face_texture_fail.as_deref() {
                    Some("panic") => {
                        r = Err(err(-32603, "InternalError", json!({"reason": "exec panicked"})))
                    }
                    Some("mismatch") => {
                        r = Err(err(
                            -32603,
                            "TextureWriteMismatch",
                            json!({"reason": "face 0: the read-back differs", "maybeExecuted": true}),
                        ))
                    }
                    _ => {}
                }
            }
            if r.is_ok() || landed {
                w.push_undo("MCP: set face texture", Some(snapshot));
            }
            Some(ok(r.map(|v| with_doc(w, v))))
        }
        "mesh.setMaterial" => {
            let snapshot = w.snapshot();
            let r = edit_gate(w, params).and_then(|_| op_mesh_set_material(w, params));
            if r.is_ok() {
                w.push_undo("MCP: set mesh material", Some(snapshot));
            }
            Some(ok(r.map(|v| with_doc(w, v))))
        }
        "edit.batch" => Some(builtin_edit_batch(w, params)),
        "node.delete" => Some(ok(
            edit_gate(w, params).and_then(|_| op_node_delete(w, params)).map(|v| with_doc(w, v))
        )),
        // m2.18 prop writes: same gate order as the other mutators (editable -> expectDoc -> op).
        "node.setTransform" => Some(ok(
            edit_gate(w, params).and_then(|_| op_set_transform(w, params)).map(|v| with_doc(w, v))
        )),
        "entity.addConnections" => Some(ok(
            edit_gate(w, params).and_then(|_| op_add_connections(w, params)).map(|v| with_doc(w, v))
        )),
        "entity.removeConnections" => Some(ok(
            edit_gate(w, params)
                .and_then(|_| op_remove_connections(w, params))
                .map(|v| with_doc(w, v))
        )),
        // m2.19 brush-entity pair (WP-T): the same gate order as every other mutator
        // (editable -> expectDoc -> op).
        "entity.tieMeshes" => Some(ok(
            edit_gate(w, params).and_then(|_| op_tie_meshes(w, params)).map(|v| with_doc(w, v))
        )),
        "entity.untie" => Some(ok(
            edit_gate(w, params).and_then(|_| op_untie(w, params)).map(|v| with_doc(w, v))
        )),
        "undo.undo" => Some(ok(
            edit_gate(w, params).and_then(|_| op_undo(w)).map(|v| with_doc(w, v))
        )),
        "map.save" => Some(ok(op_map_save(w, params))),
        "map.reload" => Some(ok(op_map_reload(w, params))),
        "map.open" => Some(ok(op_map_open(w, params))),
        _ => None,
    }
}

/// `edit.batch {ops:[{method,params}]}`: one `MCP: batch` group, `$<index>` handle
/// substitution, stop at the first failure into `-32012 PartialFailure {results, failedIndex}`
/// — no rollback (earlier ops stay applied). The `ops` whitelist is `entity.create` +
/// `entity.setKeyValues` (m2.8, unchanged by m2.15).
fn builtin_edit_batch(w: &mut HammerWorld, params: &Value) -> Reply {
    if let Err(e) = edit_gate(w, params) {
        return (None, Some(e), false);
    }
    let Some(ops) = params.get("ops").and_then(Value::as_array) else {
        return (None, Some(err(-32602, "invalid params: ops", json!({}))), false);
    };
    // the undo label is decided from the ops *listed*, before the group is opened (the bridge's
    // `batch_has_create` in mcp_bridge.c, 35528e8; protocol-diff-m2.15.md item 16): a batch with
    // no `entity.create` op carries the keyvalue label, which hammer-mcp's `undo_last` leaves the
    // unsaved-edit counter alone for — a popped set-only batch reverts nothing. A batch with no
    // readable ops at all also takes the keyvalue label (nothing was created).
    let label = if ops
        .iter()
        .any(|op| op.get("method").and_then(Value::as_str) == Some("entity.create"))
    {
        "MCP: batch"
    } else {
        "MCP: set keyvalues"
    };
    // the undo group is opened before the ops run; whether a create actually *ran* (the node
    // snapshot) is only known as the ops execute — a refused create mutated nothing
    let snapshot = w.snapshot();
    let mut handles: Vec<(String, i64)> = vec![];
    let mut results: Vec<Value> = vec![];
    let mut failed: Option<usize> = None;
    let mut any_ok = false;
    let mut created = false;
    for (i, op) in ops.iter().enumerate() {
        let method = op.get("method").and_then(Value::as_str).unwrap_or("");
        let op_params: Value = op
            .get("params")
            .filter(|p| p.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = match method {
            // (result, optional handle issued by a create op)
            "entity.create" => op_create(w, &op_params).map(|(v, h)| (v, Some(h))),
            "entity.setKeyValues" => op_set_keyvalues(w, &op_params, &handles).map(|v| (v, None)),
            other => Err(err(-32011, "NotImplemented", json!({"reason": other}))),
        };
        match outcome {
            Ok((result, handle)) => {
                any_ok = true;
                if method == "entity.create" {
                    created = true;
                }
                if let Some(handle) = handle {
                    // handle table index = the index of the create op that issued it
                    // (`handles[hcount++]` in mcp_bridge.c)
                    let id = w.nodes.last().map(|n| n.node_id).unwrap_or(1);
                    handles.push((handle, id));
                }
                results.push(result);
            }
            Err(op_err) => {
                results.push(json!({"error": op_err}));
                failed = Some(i);
                break;
            }
        }
    }
    if any_ok {
        // m2.18 RecordNodeTree: a set-only batch's group records value undo like the standalone
        // setKeyValues group does under the same knob — a popped keyvalue group restores the
        // values (review w2b #2/#5: undo_last treats both row shapes as keyvalue groups).
        w.push_undo(
            label,
            if created || w.value_undo_recorded {
                Some(snapshot)
            } else {
                None
            },
        );
    }
    match failed {
        Some(idx) => (
            None,
            Some(err(
                -32012,
                "PartialFailure",
                json!({"results": results, "failedIndex": idx}),
            )),
            false,
        ),
        None => (
            Some(with_doc(w, json!({"results": results, "failedIndex": null}))),
            None,
            false,
        ),
    }
}
