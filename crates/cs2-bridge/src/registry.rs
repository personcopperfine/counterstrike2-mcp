//! The C bridge's single method table and the module-verification gate, ported from
//! `mcp_bridge.c` (frozen, m2.20): the NEED_* bits and the `Method` record
//! (`mcp_bridge.c:1030-1051`), the 84-row `k_methods[]` table (`:1053-1143`), `find_method`
//! (`:1144-1147`), the module states and `verified_mask` (`:1149-1160`), `mod_state_name`
//! (`:1170-1176`) and the dispatch gate (`exec_request:5657-5698`, `ipc_thread:6601-6623`).
//!
//! This is the registry half of WP2.C. The replies themselves (capabilities, status, ping,
//! version, and the gate error objects) live in [`crate::control`]; the verified-module state
//! and the runtime numbers arrive as plain input structs ([`ModuleView`], a verified mask) -
//! WP3.A and WP2.A will fill them. No sockets, no threads.
//!
//! # Byte identity
//!
//! The table equals the C's `k_methods[]` row for row; the differential test
//! (`crates/cs2-bridge/tests/registry.rs`) compiles the C table out of the frozen source with
//! `cc` and compares every field of every row. The first-unmet-need order of [`gate`] is the
//! C's `bits[]`/`mods[]` pair (`exec_request`, `mcp_bridge.c:5676-5678`) - the order decides
//! which module the `-32017 PrerequisiteFailed` data object names.
//!
//! # Numbers
//!
//! The NEED_* values are the C's `#define`s (`mcp_bridge.c:1030-1044`) - protocol numbers, not
//! ABI offsets. The pin indices come from the generated
//! [`crate::generated::pins`] (`PIN_TF2` .. `PIN_PET`), never hand-written.

/// `NEED_TF2` (`mcp_bridge.c:1030`): toolframework2.dll.
pub const NEED_TF2: u32 = 1;
/// `NEED_HAMMER` (`mcp_bridge.c:1031`): hammer.dll.
pub const NEED_HAMMER: u32 = 2;
/// `NEED_SCENE` (`mcp_bridge.c:1032`): scenesystem.dll.
pub const NEED_SCENE: u32 = 4;
/// `NEED_ENGINE` (`mcp_bridge.c:1033`): engine2.dll.
pub const NEED_ENGINE: u32 = 8;
/// `NEED_RSV` (`mcp_bridge.c:1034`): rendersystemvulkan.dll.
pub const NEED_RSV: u32 = 16;
/// `NEED_RENDER` (`mcp_bridge.c:1035`): scene + engine + rendersystem together.
pub const NEED_RENDER: u32 = NEED_SCENE | NEED_ENGINE | NEED_RSV;
/// `NEED_CLIENT` (`mcp_bridge.c:1036`): client.dll.
pub const NEED_CLIENT: u32 = 32;
/// `NEED_INPUT` (`mcp_bridge.c:1037`): client + engine (the input hooks live in client.dll).
pub const NEED_INPUT: u32 = NEED_CLIENT | NEED_ENGINE;
/// `NEED_SERVER` (`mcp_bridge.c:1038`): server.dll.
pub const NEED_SERVER: u32 = 64;
/// `NEED_TIER0` (`mcp_bridge.c:1039`): tier0.dll.
pub const NEED_TIER0: u32 = 128;
/// `NEED_SCHEMA` (`mcp_bridge.c:1040`): schemasystem.dll (runtime field offsets).
pub const NEED_SCHEMA: u32 = 256;
/// `NEED_SOUND` (`mcp_bridge.c:1041`): soundsystem.dll (ISoundOpSystem hook).
pub const NEED_SOUND: u32 = 512;
/// `NEED_MODELDOC` (`mcp_bridge.c:1042`): modeldoc_editor.dll (.vmdl document lifecycle, m2.11).
pub const NEED_MODELDOC: u32 = 1024;
/// `NEED_PET` (`mcp_bridge.c:1043`): pet.dll (.vpcf document lifecycle + operator registry, m2.12).
pub const NEED_PET: u32 = 2048;
/// Rust-only: a KHook `IKHook` API is installed in this process
/// (`s2sdk::hook::api().is_some()`), so a row that installs a **KHook** hook can run.
///
/// The C bridge gets its `IKHook` from `ISmmAPI::GetDetourInterface` at `Load` and every hook row
/// assumes it; this build also runs in the tool role without Metamod (and keeps running after a
/// Metamod unload), so the same rows have to say what they are waiting for. `4096` is the next
/// free bit after the C's `NEED_PET` (2048); no C row names it.
///
/// Unlike the module bits, this one is not derived from a pin and has no `PIN_*` entry: the
/// gate asks [`s2sdk::hook::api`] directly ([`gate`]), so there is no "KHook module" in the
/// module tables. An absent API answers `-32017 PrerequisiteFailed` with
/// `{"blockedOn":"metamod: KHook not loaded"}` (see [`crate::control::khook_missing`]).
///
/// # Which rows may carry it, and which must not
///
/// Only a row that **registers a KHook hook** may name this bit: `game.saveRestore` (its
/// `AllowSaveRestore` virtual), `input.set` (the two CCSGOInput slots) and `sound.subscribe`
/// (the three `StartSoundEvent` slots). Those three are the rows whose body always installs
/// something; `sound.status`/`sound.drain` may arm the wrap too when the API is present (below),
/// which is why the row-level bit cannot be narrowed to the request.
///
/// The rows that *do not* install are the reason this rule matters: `input.clear`,
/// `input.status`, `input.uninstall`, `sound.status`, `sound.drain`, `sound.uninstall` and
/// `control.*` are the **off-ramps**. A row that only reads state or turns a hook off must keep
/// answering after a Metamod unload, or a user who armed the input override could no longer
/// clear or uninstall it (`-32017` with no game running behind it), leaving buttons forced. So
/// they answer [`Gate::Run`] without KHook and never register one. `input.status`/`input.clear`
/// do not reach `in_install` at all. The C's `sound.status`/`sound.drain` do run its `mode`
/// sink selection and clients rely on the first of them arming the wrap, so those two run it
/// **while the API is present** and otherwise answer their normal reply with a `blockedOn`
/// note - no install, no error (PROTOCOL.md §18.2, §4.2).
pub const NEED_KHOOK: u32 = 4096;

/// The `blockedOn` text of the [`NEED_KHOOK`] prerequisite. Metamod is where the `IKHook` comes
/// from (`ISmmAPI::GetDetourInterface`), so the answer names it.
pub const KHOOK_BLOCKED_ON: &str = "metamod: KHook not loaded";

/// The `blockedOn` text of a row's own "Metamod is not loaded" prerequisite, as
/// [`crate::ops::metamod::plugins_answer`] answers it today and a future
/// [`crate::registry::gate_with`] arm would.
pub const METAMOD_BLOCKED_ON: &str = "metamod: not loaded";

/// Every base bit a row's `needs` may name (the C's `bits[]` gate table,
/// `mcp_bridge.c:5676`), in that table's order - the order decides which module a
/// `-32017 PrerequisiteFailed` names first.
const GATE_BITS: [u32; 12] = [
    NEED_TF2, NEED_SCENE, NEED_ENGINE, NEED_RSV, NEED_HAMMER, NEED_CLIENT, NEED_SERVER, NEED_TIER0,
    NEED_SCHEMA, NEED_SOUND, NEED_MODELDOC, NEED_PET,
];

/// The pin index each [`GATE_BITS`] entry pairs with (`mcp_bridge.c:5677`, the C's `mods[]`).
/// The two tables must stay parallel - the C holds this with a `_Static_assert` on their byte
/// sizes; here the array lengths are part of the types.
const GATE_PINS: [u8; 12] = [
    pins::PIN_TF2,
    pins::PIN_SCENESYSTEM,
    pins::PIN_ENGINE2,
    pins::PIN_RSV,
    pins::PIN_HAMMER,
    pins::PIN_CLIENT,
    pins::PIN_SERVER,
    pins::PIN_TIER0,
    pins::PIN_SCHEMA,
    pins::PIN_SOUND,
    pins::PIN_MODELDOC,
    pins::PIN_PET,
];

/// Every base bit, as [`verified_mask`] pairs it with a pin (`mcp_bridge.c:1153-1160`, the C's
/// own `verified_mask` order - different from [`GATE_BITS`], same mask).
const MASK_ORDER: [(u32, u8); 12] = [
    (NEED_TF2, pins::PIN_TF2),
    (NEED_HAMMER, pins::PIN_HAMMER),
    (NEED_SCENE, pins::PIN_SCENESYSTEM),
    (NEED_ENGINE, pins::PIN_ENGINE2),
    (NEED_RSV, pins::PIN_RSV),
    (NEED_CLIENT, pins::PIN_CLIENT),
    (NEED_SERVER, pins::PIN_SERVER),
    (NEED_TIER0, pins::PIN_TIER0),
    (NEED_SCHEMA, pins::PIN_SCHEMA),
    (NEED_SOUND, pins::PIN_SOUND),
    (NEED_MODELDOC, pins::PIN_MODELDOC),
    (NEED_PET, pins::PIN_PET),
];

use crate::generated::pins;

/// One row of the method table - the C's `Method` record (`mcp_bridge.c:1045-1051`).
#[derive(Debug, Eq, PartialEq)]
pub struct Method {
    /// The method name (`"doc.active"`, ...).
    pub name: &'static str,
    /// Required modules (verified); `0` = the `control.*` fast path.
    pub needs: u32,
    /// `1` = implemented, `0` = documented but blocked.
    pub implemented: bool,
    /// Never advertised (`control.hello`); only `control.hello` itself is hidden.
    pub hidden: bool,
    /// Where the design for a blocked method lives (`mcp_bridge.c:1106` et al.); `None` is the
    /// C's `0`.
    pub blocked_on: Option<&'static str>,
}

/// The C's `k_methods[]` (`mcp_bridge.c:1053-1143`), row for row, in the same order.
pub static METHODS: &[Method] = &[
    Method { name: "control.hello", needs: 0, implemented: true, hidden: true, blocked_on: None },
    Method { name: "control.ping", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "control.status", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "control.capabilities", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "control.version", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "control.cancel", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "control.shutdown", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "doc.active", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "view.render", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* task 13: offscreen Hammer render -> PNG */
    Method { name: "world.render", needs: NEED_RENDER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "input.set", needs: NEED_INPUT | NEED_KHOOK, implemented: true, hidden: false, blocked_on: None },   /* virtual buttons + view angles (inputhook-scratch.md) */
    Method { name: "input.clear", needs: NEED_INPUT, implemented: true, hidden: false, blocked_on: None },
    Method { name: "input.status", needs: NEED_INPUT, implemented: true, hidden: false, blocked_on: None },
    Method { name: "input.uninstall", needs: NEED_INPUT, implemented: true, hidden: false, blocked_on: None },
    Method { name: "test.checkpoint", needs: NEED_TIER0, implemented: true, hidden: false, blocked_on: None },   /* ITestScriptMgr::CheckPoint (vtest runner) */
    Method { name: "test.active", needs: NEED_TIER0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "console.exec", needs: NEED_ENGINE, implemented: true, hidden: false, blocked_on: None },   /* IInputService cbuf; -testscript or -tools sessions only */
    Method { name: "game.tick", needs: NEED_ENGINE, implemented: true, hidden: false, blocked_on: None },
    Method { name: "game.saveRestore", needs: NEED_SERVER | NEED_KHOOK, implemented: true, hidden: false, blocked_on: None },   /* unlock save/load (AllowSaveRestore -> true) */
    Method { name: "game.local", needs: NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* local player: view, origin, velocity, active weapon (mcp_bridge_game.inc) */
    Method { name: "game.weapons", needs: NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* inventory: clips, reserve, def index per weapon */
    Method { name: "game.radar", needs: NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* all controllers/pawns, bomb, hostages, spotted flags */
    Method { name: "game.rules", needs: NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* C_CSGameRules via cs_gamerules: warmup, freeze, round state (schema-resolved, nullable) */
    Method { name: "game.entities", needs: NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* identities filtered by class (designer name) and/or name (targetname), max 256 */
    Method { name: "game.selectWeapon", needs: NEED_ENGINE, implemented: true, hidden: false, blocked_on: None },   /* slotN / inventory commands on the cbuf; weapon/defIndex/handle -> CCSGOInput MakeWeaponSelection (needs client+schema) */
    Method { name: "game.schemaProbe", needs: NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* diagnostics: which module names have a schema scope */
    Method { name: "sound.status", needs: NEED_SOUND, implemented: true, hidden: false, blocked_on: None },   /* reports the wrap/listener state and the ring's counters; installs the listener half only (it needs no KHook) */
    Method { name: "sound.subscribe", needs: NEED_SOUND | NEED_KHOOK, implemented: true, hidden: false, blocked_on: None },   /* the group's arming row: mode wrap|listener|both (installs the hooks) */
    Method { name: "sound.drain", needs: NEED_SOUND, implemented: true, hidden: false, blocked_on: None },   /* ring buffer of started sound events */
    Method { name: "sound.uninstall", needs: NEED_SOUND, implemented: true, hidden: false, blocked_on: None },
    Method { name: "node.list", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* live tree walk: nodeID/class/origin */
    Method { name: "node.delete", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* CMapDoc::DeleteNode by nodeID (undoable) */
    Method { name: "mesh.create", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* polygon soup -> welded CMapMesh (undoable) */
    Method { name: "mesh.setGeometry", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* replace a mesh's polygons (undoable) */
    Method { name: "mesh.setMaterial", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* all faces -> material (undoable) */
    Method { name: "doc.list", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("bridge-protocol.md §7 doc.list (not implemented in M2.2)") },
    Method { name: "map.save", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "map.reload", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "map.open", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* OpenFile(addon-content-relative maps/x.vmap, flags 1), gated: path shape + host file + binary-v9 DMX header + session-leak (m2.16) */
    Method { name: "map.run", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("hammer-save-compile.md §B (phase-3 task 5)") },
    Method { name: "undo.undo", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "entity.create", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "entity.setKeyValues", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "node.setTransform", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* m2.18: SetScales/SetAngles/SetOrigin (slots 19/18/17, re-props.md Q2) */
    Method { name: "entity.addConnections", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },  /* m2.18: CMapEntityIONode::AddConnection (re-props.md Q3) */
    Method { name: "entity.removeConnections", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None }, /* m2.18: RemoveConnection in descending index order */
    Method { name: "entity.tieMeshes", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* m2.19: tie meshes into a new/existing brush entity, one group (re-triggers.md Q1) */
    Method { name: "entity.untie", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* m2.19: CMapDoc::MoveToWorld (opens its own undo group) */
    Method { name: "edit.batch", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "file.open_asset", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by map.open (m2.16): the VToolMessage asset route is not an open/reload primitive (file-edit-reload-compile.md §B)") },
    Method { name: "job.status", needs: 0, implemented: false, hidden: false, blocked_on: Some("bridge-protocol.md §7 (phase-3 task 5)") },
    /* documented-but-blocked rows (protocol §7): -32011 NotImplemented + blockedOn */
    Method { name: "undo.redo", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* m2.17: GetRedoHistory(doc) -> its vtable slot 2 (the pair partner's apply-and-pop) */
    Method { name: "entity.get", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* live entity read by nodeID (m2.15) */
    Method { name: "entity.find", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },   /* live entity query by class/targetname/within (m2.15) */
    Method { name: "entity.delete", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by node.delete (m2.5, by nodeID)") },
    Method { name: "selection.get", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("bridge-protocol.md §7 selection.* (not isolated)") },
    Method { name: "selection.set", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("bridge-protocol.md §7 selection.* (not isolated)") },
    Method { name: "mesh.setFaceRect", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)") },
    Method { name: "mesh.createFaceFromPoints", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)") },
    Method { name: "mesh.moveVertices", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)") },
    Method { name: "mesh.extrudeFaces", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)") },
    Method { name: "mesh.deleteFaces", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)") },
    Method { name: "mesh.get", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("reads come from the saved .vmap (MCP get_mesh); node.list enumerates live nodes") },
    Method { name: "material.assign", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by mesh.setMaterial (m2.5)") },
    Method { name: "modeldoc.active", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc first pass (m2.11): active .vmdl document */
    Method { name: "modeldoc.open", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc: OpenFile(addon-content-relative path, flags 1) */
    Method { name: "modeldoc.reload", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc: ReloadDocument (no prompt; discards in-editor changes) */
    Method { name: "modeldoc.save", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc: SaveAndNotify(path,path,0,0) (the FileSave call) */
    Method { name: "modeldoc.compile", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc: CModelDocEditorSession::Compile(session, 0) */
    Method { name: "modeldoc.nodeTypes", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc: the registered node-type descriptor table */
    Method { name: "modeldoc.render", needs: NEED_MODELDOC, implemented: true, hidden: false, blocked_on: None },   /* ModelDoc vision pass (m2.14): main viewport render-to-memory (CToolScene::Render) -> PNG */
    /* particle editor first pass (m2.12): .vpcf. pet.load is the only method that can run before pet.dll exists (it loads it); everything else pins pet.dll, pet.schema the schema system too. */
    Method { name: "pet.load", needs: NEED_TF2, implemented: true, hidden: false, blocked_on: None },   /* LoadToolSystem(idx from GetToolIndexByName("pet")) */
    Method { name: "pet.active", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* active .vpcf document: path/arrays/classes */
    Method { name: "pet.open", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* OpenFile(addon-content-relative path, flags 1), gated: vpcf_check on the resolved host file */
    Method { name: "pet.reload", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* ReloadDocument, gated: vpcf_check on GetPath first (malformed input wedges the main thread) */
    Method { name: "pet.save", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* Save(path, path, 0, 0) (the FileSave call) */
    Method { name: "pet.functionTypes", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* particles.dll IParticleSystemMgr: the operator registry */
    Method { name: "pet.schema", needs: NEED_PET | NEED_SCHEMA, implemented: true, hidden: false, blocked_on: None },   /* particles.dll type scope: class fields/bases/metadata */
    Method { name: "pet.text", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* the live document serialized (SaveToFile to a private path) + dirty/undo flags (m2.13) */
    Method { name: "pet.apply", needs: NEED_PET | NEED_TIER0, implemented: true, hidden: false, blocked_on: None },   /* replace the live tree from KV3 text as ONE pet undo step (gated: vpcf_check) */
    Method { name: "pet.undo", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* CPetDoc::Undo (pet's own history) */
    Method { name: "pet.redo", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* CPetDoc::Redo */
    Method { name: "pet.render", needs: NEED_PET, implemented: true, hidden: false, blocked_on: None },   /* preview viewport render-to-memory (CToolScene::Render) -> PNG */
    Method { name: "job.cancel", needs: 0, implemented: false, hidden: false, blocked_on: Some("bridge-protocol.md §7 job.cancel (reserved)") },
];

/// `METHOD_COUNT` (`mcp_bridge.c:1144`), checked against [`METHODS`] by the tests.
pub const METHOD_COUNT: usize = 84;

/// `find_method` (`mcp_bridge.c:1144-1147`): a linear scan, like the C - over this build's
/// overrides ([`RUST_OVERRIDES`], which replace a C row), then the C table, then this build's
/// own rows ([`RUST_METHODS`]).
pub fn find(name: &str) -> Option<&'static Method> {
    RUST_OVERRIDES
        .iter()
        .find(|m| m.name == name)
        .or_else(|| METHODS.iter().find(|m| m.name == name))
        .or_else(|| RUST_METHODS.iter().find(|m| m.name == name))
}

/// The row this build uses in place of the C row `m`: its [`RUST_OVERRIDES`] entry if it has
/// one, else `m` itself. Every consumer that walks [`METHODS`] for behaviour reads the table
/// through this, so an override is seen everywhere `find` is.
pub fn effective(m: &'static Method) -> &'static Method {
    RUST_OVERRIDES.iter().find(|o| o.name == m.name).unwrap_or(m)
}

/// The table this build actually runs, in advertisement order: the C rows (each replaced by
/// its [`RUST_OVERRIDES`] entry when it has one), then [`RUST_METHODS`].
pub fn effective_methods() -> impl Iterator<Item = &'static Method> {
    METHODS.iter().map(effective).chain(RUST_METHODS.iter())
}

/// Rows of the C table this build replaces - never in [`METHODS`], which stays byte-identical
/// to the frozen C source (`tests/registry.rs` pins it row for row, `implemented` and
/// `blockedOn` included).
///
/// Each row names an **existing** C row and supersedes it for this build: [`find`], [`gate`],
/// [`crate::dispatch::decide`] and the advertisement ([`crate::control`]) read "override if
/// present, else the C row". At most one override per name, and a name is never also in
/// [`RUST_METHODS`] (`tests/overrides.rs` holds all three). The reason for each override:
///
/// * `map.run`: stays blocked, with a `blockedOn` that says what superseded it
///   (hammer-mcp `run_map` over the VToolMessage `command` channel), not the C's phase-3 note.
/// * `job.status`, `job.cancel`: implemented (bridge m3.12, [`crate::ops::job`]). `needs` stays
///   `0` like the C's blocked rows: the rows are answered on the IPC lane from the job table,
///   not queued (the lane-served exception to "implemented `needs == 0` is `control.*`").
/// * `selection.get`, `selection.set`: implemented (bridge m3.12, [`crate::ops::selection`]) over
///   the selection functions the m3.12 RE pinned; the C's "not isolated" block is gone. Queued
///   main-thread Hammer ops, `needs` unchanged.
/// * `mesh.get`: implemented (bridge m3.12, [`crate::ops::mesh`]): a queued main-thread read of
///   the live mesh over Hammer's face/edge accessors (the C row said "reads come from the saved
///   .vmap"). `needs` stays `NEED_HAMMER`.
pub static RUST_OVERRIDES: &[Method] = &[
    Method { name: "map.run", needs: NEED_HAMMER, implemented: false, hidden: false, blocked_on: Some("superseded by hammer-mcp run_map (VToolMessage command channel)") },
    Method { name: "job.status", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "job.cancel", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "selection.get", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "selection.set", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "mesh.get", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None }
];

/// This build's own method rows - never in [`METHODS`], which keeps the retired C table's 84
/// rows (`tests/registry.rs`).
///
/// `metamod.plugins` is the first, `job.start` the second (a lane-answered row too, with
/// `job.status`/`job.cancel` as [`RUST_OVERRIDES`]): a status row over Metamod's plugin table
/// ([`crate::ops::metamod`]), served on the IPC lane from a snapshot - which is why it
/// carries `needs == 0` (a queued row would need a tick; see the module's docs for the two
/// `blockedOn` prerequisites the row answers itself). That `needs == 0` is a property of a
/// lane-answered row, not of living in this table: the rows answered on the lane are those
/// whose op says so via `serves`, and a future Rust row that needs a module bit may name one
/// like any C row. The lane answers a served name before the C's `find_method` chain would
/// matter, and [`crate::dispatch::is_ported`] serves it, so `decide`/`gate` stay honest for
/// it too.
pub static RUST_METHODS: &[Method] = &[
    Method { name: "metamod.plugins", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "job.start", needs: 0, implemented: true, hidden: false, blocked_on: None },
    Method { name: "mesh.setFaceTexture", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "node.createPrefab", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "node.prefabFromSelection", needs: NEED_HAMMER, implemented: true, hidden: false, blocked_on: None },
    Method { name: "cvar.get", needs: NEED_TIER0, implemented: true, hidden: false, blocked_on: None }
];

/// The C's module states `MS_*` (`mcp_bridge.c:1149`, an enum): `not_loaded`, `verifying`,
/// `verified`, `mismatch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModState {
    /// `MS_NOT_LOADED`.
    NotLoaded,
    /// `MS_VERIFYING`.
    Verifying,
    /// `MS_VERIFIED`.
    Verified,
    /// `MS_MISMATCH`.
    Mismatch,
}

impl ModState {
    /// `mod_state_name` (`mcp_bridge.c:1170-1176`): the JSON capability string.
    pub fn as_str(self) -> &'static str {
        match self {
            ModState::Verifying => "verifying",
            ModState::Verified => "verified",
            ModState::Mismatch => "mismatch",
            ModState::NotLoaded => "not_loaded",
        }
    }
}

/// One pin module's verified-module state, as [`verified_mask`] / [`crate::control`] read it.
///
/// A slice of these, one per pin module in pin order (`PIN_MODULES`' order, `hammer.dll`
/// first), is the plain input struct WP3.A fills. The C's `mod_ok` also requires a non-null
/// module base (`mcp_bridge.c:1151`); that fold happens where the states are produced - a
/// [`ModState::Verified`] here means "verified and based", exactly what `mod_ok` answered.
///
/// `relaxed` names the pins the KHook-aware identity check had to relax for this module (the
/// KHook hook's own virtual slot, a byte pin another plugin has an inline detour on); see
/// [`crate::modules::slot_matches`]. It is the `"relaxed"` array of the capabilities'
/// module object, and empty for a module that needed no relaxation. `ModuleView` is not `Copy`
/// for this field's sake; `ModuleView::new` is the empty-list constructor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleView {
    /// The module's state.
    pub state: ModState,
    /// The pins the identity check relaxed for this module, in the order it met them.
    pub relaxed: Vec<&'static str>,
}

impl ModuleView {
    /// A view with one state and no relaxed pins.
    #[must_use]
    pub fn new(state: ModState) -> Self {
        ModuleView {
            state,
            relaxed: Vec::new(),
        }
    }

    /// A view with one state and the pins the identity check relaxed.
    #[must_use]
    pub fn with_relaxed(state: ModState, relaxed: Vec<&'static str>) -> Self {
        ModuleView { state, relaxed }
    }
}

/// The C `verified_mask` (`mcp_bridge.c:1152-1160`): each module that is verified contributes
/// its NEED bit.
///
/// `modules` must be in pin order and exactly [`crate::generated::pins::PIN_MODULE_COUNT`]
/// long - the C indexes `g_modState` by the `PIN_*` constants; a wrong-length slice fails
/// loudly instead of guessing.
pub fn verified_mask(modules: &[ModuleView]) -> u32 {
    assert_eq!(
        modules.len(),
        pins::PIN_MODULE_COUNT,
        "modules must carry one view per pin module, in PIN_MODULES order"
    );
    let mut mask = 0u32;
    for &(bit, pin) in &MASK_ORDER {
        if modules[usize::from(pin)].state == ModState::Verified {
            mask |= bit;
        }
    }
    mask
}

/// The dispatch gate's outcome for one request - what the C answers for a method name and a
/// verified mask.
///
/// * [`Gate::Unknown`]: no such method - the C answers `-32601 MethodNotFound` both for a
///   `control.*` name that is not in the table and for any other unknown name
///   (`mcp_bridge.c:6601-6605`, `:6607-6610`).
/// * [`Gate::Blocked`]: the row is `implemented = 0` - the C answers `-32011 NotImplemented`
///   with `{"blockedOn":...}` (`:6611-6616`, `exec_request:5668-5674`).
/// * [`Gate::Prerequisite`]: implemented, but a `needs` module is not verified - the C answers
///   `-32017 PrerequisiteFailed` on the main thread (`exec_request:5675-5698`). The named
///   module is the FIRST unmet need in the C's `bits[]` order, falling back to
///   `PIN_HAMMER`/`hammer.dll` when no bit matches (`:5680-5685`) - the same default the C
///   ships.
/// * [`Gate::KhookMissing`]: implemented, every module verified, but the row needs
///   [`NEED_KHOOK`] and no `IKHook` is installed. `-32017` again, with the `blockedOn` text
///   [`KHOOK_BLOCKED_ON`] - it is not a module, so it cannot name a module state.
/// * [`Gate::Run`]: dispatch. In the C this is: the `control.*` fast path in `ipc_thread`
///   (`:6497-6623`), or the main thread executes the op after the gate.
#[derive(Debug, Eq, PartialEq)]
pub enum Gate {
    /// Dispatch (see above).
    Run,
    /// No such method: `-32601 MethodNotFound`.
    Unknown,
    /// Documented but blocked: `-32011 NotImplemented`, data `{"blockedOn":...}`.
    Blocked {
        /// The row's `blocked_on` string, or the C's `"unknown"` fallback
        /// (`mcp_bridge.c:6613`, `:5672`).
        blocked_on: &'static str,
    },
    /// A needs module is not verified: `-32017 PrerequisiteFailed`, data
    /// `{"module":...,"check":"identity","state":...}` (`exec_request:5693-5696`).
    Prerequisite {
        /// The module's name, from [`crate::generated::pins::PIN_MODULES`] - never retyped.
        module: &'static str,
        /// The module's pin index (the generated `PIN_*` value).
        pin: u8,
    },
    /// All modules are verified, but a row that registers a KHook hook ran without an `IKHook`:
    /// `-32017 PrerequisiteFailed`, data `{"blockedOn":"metamod: KHook not loaded"}`.
    KhookMissing,
}

/// The dispatch gate: what does the C answer for `name` under this verified mask?
///
/// A `needs == 0` implemented row is [`Gate::Run`] - those rows are exactly the `control.*`
/// methods, which the C dispatches in `ipc_thread`'s fast path (`mcp_bridge.c:6497-6623`);
/// `exec_request`'s `needs == 0` arm (`:5681-5684`) answers `-32601` only for a row that could
/// never be dispatched there, and no such row exists in the table (the differential test pins
/// the whole table).
///
/// The mask check itself runs on the main thread in the C (`exec_request:5669-5698`);
/// `ipc_thread` only asks `needs != 0` before queueing. This port answers the composed
/// decision, so the -32017 path is decided here exactly as `exec_request` decides it.
pub fn gate(name: &str, verified: u32) -> Gate {
    gate_with(name, verified, s2sdk::hook::api().is_some())
}

/// [`gate`] with the KHook question answered explicitly, for a caller that is not the process
/// (the C differential's generator has no KHook gate at all, and a host test wants both answers
/// from one binary).
///
/// The order is the C's, plus [`NEED_KHOOK`] **after** the module bits: a missing module is the
/// more specific answer while it is missing (the row cannot run either way), and with every
/// module verified the missing `IKHook` is what is left. The KHook bit itself never appears in
/// `verified` - it is `khook_present` that satisfies it.
pub fn gate_with(name: &str, verified: u32, khook_present: bool) -> Gate {
    gate_row(find(name), verified, khook_present)
}

fn gate_row(row: Option<&'static Method>, verified: u32, khook_present: bool) -> Gate {
    let m = match row {
        None => return Gate::Unknown,
        Some(m) => m,
    };
    if !m.implemented {
        return Gate::Blocked {
            blocked_on: m.blocked_on.unwrap_or("unknown"), // mcp_bridge.c:6613, :5672
        };
    }
    // The module bits only: NEED_KHOOK is never in a verified mask (`verified_mask` is
    // module-driven), so it is asked separately below.
    let module_needs = m.needs & !NEED_KHOOK;
    if module_needs == 0 && m.needs & NEED_KHOOK != 0 && !khook_present {
        return Gate::KhookMissing;
    }
    if module_needs == 0 {
        return Gate::Run; // the control.* fast path
    }
    if (verified & module_needs) != module_needs {
        // The first unmet need in the C's bits[]/mods[] order; if no bit matches (impossible
        // for a row whose needs is a subset of the defined bits - the self-test pins that), the
        // C keeps its PIN_HAMMER default (exec_request:5680-5685).
        let mut pin = pins::PIN_HAMMER;
        for (&bit, &p) in GATE_BITS.iter().zip(GATE_PINS.iter()) {
            if (module_needs & bit) != 0 && (verified & bit) == 0 {
                pin = p;
                break;
            }
        }
        return Gate::Prerequisite {
            module: pins::PIN_MODULES[usize::from(pin)].name,
            pin,
        };
    }
    if m.needs & NEED_KHOOK != 0 && !khook_present {
        return Gate::KhookMissing;
    }
    Gate::Run
}
