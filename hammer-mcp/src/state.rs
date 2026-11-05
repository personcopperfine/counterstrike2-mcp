//! Per-process server state shared by all Hammer tools.
use crate::vmdl::ContentPaths;
use cs2_mcp_common::bridge::{BridgeClient, SharedBridge};
use cs2_mcp_common::config::CommonConfig;
use cs2_mcp_common::pathmap::PathMap;
use cs2_mcp_common::runtime::docker::{DockerOptions, DockerRuntime, DEFAULT_SHORT_TIMEOUT};
use cs2_mcp_common::bridge::errors::Kind as BridgeKind;
use cs2_mcp_common::tools::{Budget, Mutability, ToolError, ToolSpec, ToolState};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

/// Unsaved-edit bookkeeping (POC `_edits_since_save` / `_edits_doc_id`): mutating tools bump the
/// counter, `save_map`/reload clear it, a document switch resets it.
#[derive(Debug, Default)]
pub struct Edits {
    pub since_save: u64,
    pub doc_id: Option<String>,
    /// the last `doc.active` read showed an untitled (new) document: `save_map` always refuses it
    pub untitled: bool,
}

/// The document state one `doc.active` (or a write's own `doc` tail) reports: the document's `id`,
/// the serial of the **top** undo group (`topSerial`) and the undo stack's size (`undoDepth`), all
/// from the same reply. `create_trigger` measures how far a half-finished multi-step call moved
/// the undo depth with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocState {
    pub doc_id: Option<String>,
    pub serial: u64,
    pub depth: u64,
}

impl DocState {
    /// `doc.active`'s `(id, topSerial, undoDepth)`; `None` when either undo field is missing or
    /// malformed (`undoDepth` is signed live, so a negative one is not a depth).
    pub fn read(doc: &Value) -> Option<DocState> {
        let serial = doc.get("topSerial").and_then(Value::as_u64)?;
        let depth = doc.get("undoDepth").and_then(Value::as_u64)?;
        Some(DocState {
            doc_id: doc.get("id").and_then(Value::as_str).map(str::to_string),
            serial,
            depth,
        })
    }
}

/// Where the effective addon came from (`session_status.addon.source`, `set_addon`'s reply).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonSource {
    /// the active Hammer document lives under `content|game/csgo_addons/<addon>/`
    Document,
    /// an explicit `set_addon {addon}` (stays until `set_addon {addon:null}`)
    SetAddon,
    /// `--addon` / `CS2_MCP_ADDON`: no explicit choice and no document in an addon
    Flag,
}

impl AddonSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AddonSource::Document => "document",
            AddonSource::SetAddon => "set_addon",
            AddonSource::Flag => "flag",
        }
    }
}

/// The runtime addon selection. Resolution: an explicit `set_addon` wins; else the addon of the
/// active document (refreshed wherever a tool reads `doc.active`, and before addon-scoped calls);
/// else the `--addon` flag.
#[derive(Debug, Clone)]
pub struct AddonSel {
    pub flag: String,
    pub explicit: Option<String>,
    pub document: Option<String>,
    /// when the document-derived value was last refreshed (throttles the pre-call re-check)
    pub checked_at: Option<std::time::Instant>,
    /// the last re-check could not read `doc.active` (bridge down): back off for the throttle window
    pub check_failed: bool,
    /// the last re-check could not tell (lock wait timed out, bridge error): the document-derived
    /// value may be stale. Cleared by the next successful read.
    pub unconfirmed: bool,
}

impl AddonSel {
    pub fn new(flag: String) -> AddonSel {
        AddonSel { flag, explicit: None, document: None, checked_at: None, check_failed: false, unconfirmed: false }
    }

    pub fn current(&self) -> (String, AddonSource) {
        if let Some(a) = &self.explicit {
            (a.clone(), AddonSource::SetAddon)
        } else if let Some(a) = &self.document {
            (a.clone(), AddonSource::Document)
        } else {
            (self.flag.clone(), AddonSource::Flag)
        }
    }
}

#[derive(Clone)]
pub struct HammerState {
    pub bridge: SharedBridge,
    pub path_map: Arc<PathMap>,
    pub edits: Arc<Mutex<Edits>>,
    pub started_at: f64,
    pub launch_command: String,
    pub expect_build: Option<String>,
    /// `<project>/`: this checkout by default, its repo files under `cs2_mcp_common::mcp_repo_dir`.
    pub project_root: PathBuf,
    /// The game tree root (`--app-root`; default `<project>/workshop/run/app`, the container's).
    pub app_root: PathBuf,
    /// podman access for `model_compile` (exec into the ModelDoc container; never launches one).
    pub runtime: Arc<DockerRuntime>,
    /// The addon selection (`--addon` is only the fallback; see [`AddonSel`]): read it through
    /// [`HammerState::addon`] / [`HammerState::content`], never directly.
    pub addon_sel: Arc<RwLock<AddonSel>>,
    /// `--tools-container` override; default: discover by the label `cs2mcp.tool=modeldoc`.
    pub tools_container: Option<String>,
    /// FGD search root (`<project>/workshop/run/app/game`): `csgo/csgo.fgd` + `@include`s
    /// resolved in `csgo/` then `core/` (`crate::fgd`). Tests point it at a fixture tree.
    pub fgd_root: PathBuf,
    /// The server-wide write gate (review-5 L1): one async mutex held by a tool call for its whole
    /// bridge sequence and by nothing else. See [`HammerState::write_gate`] for the handshake.
    pub gate: Arc<AsyncMutex<()>>,
    /// The server-wide VToolMessage post gate (review-1 M7): `vtmsend.exe` reads ONE fixed payload
    /// file (`<app>/vtm_payload.kv3`; the program takes no path argument), so two posts that each
    /// write-then-exec must never interleave. `run_map` holds it across its whole post sequence —
    /// the `disconnect` of a remount, the mode lines and the map command — which also makes its
    /// "a load is already in flight" check and record atomic (review-1 M6).
    pub vtm_gate: Arc<AsyncMutex<()>>,
    /// What this server's last `run_map` did ([`crate::tools::run_map::LastLoad`]): the engine load
    /// it started, whether a terminal console line was seen, and the map the engine may still hold
    /// mounted — the fallback when the bridge exposes no `game.local` (review-1 H1/M6).
    pub last_load: Arc<Mutex<Option<crate::tools::run_map::LastLoad>>>,
    /// The last first-map `open_asset` post of `open_map` (addon-relative path, when) whose
    /// document has not been confirmed yet. While it is younger than
    /// [`crate::tools::lifecycle::OPEN_PENDING_WINDOW`], a second vtm open is refused
    /// (`OpenPending`, nothing sent): Hammer may still be loading, or a modal may be up.
    pub pending_vtm_open: Arc<Mutex<Option<(String, std::time::Instant)>>>,
    /// The background map-compile jobs of `compile_map` (`tools/compile_map.rs`): bounded at 8,
    /// at most one running (the check-and-insert is one operation on the store's own mutex,
    /// `CompileJobs::start`); the run task itself is supervised and jobs running past their
    /// deadline + reap grace are reaped, so a lost watcher can never block later starts.
    pub compile_jobs: Arc<crate::tools::compile_map::CompileJobs>,
    /// The model catalog (`tools/model_catalog.rs`): built lazily on first use inside
    /// `tokio::task::spawn_blocking` (the two VPK parses and the addon walks are blocking),
    /// refreshed when the csgo `pak01_dir.vpk` mtime moves, and rescanned (addon dirs only,
    /// VPKs reused) when a model lookup misses — addon files change during a session.
    /// `list_models` serves the cached value through `tools::model_catalog::catalog_for`.
    pub model_catalog: Arc<Mutex<Option<Arc<crate::tools::model_catalog::ModelCatalog>>>>,
    /// The soundevent index (`crate::soundevents::index`): built lazily on first use, served by
    /// `soundevents::index_for`; `soundevent_index_invalidate` drops it after an addon write.
    pub soundevent_index: Arc<Mutex<Option<Arc<crate::soundevents::SoundEventIndex>>>>,
    /// The index `soundevent_index_invalidate` just dropped (review M3): `index_for` takes it as
    /// the previous index and **rescans the addon** from it (`rescan_addon`, the 193 decoded game
    /// VPK blocks reused) while both game VPKs are untouched, instead of re-decoding them. Cleared
    /// as soon as a fresh index lands in `soundevent_index`.
    pub soundevent_index_last: Arc<Mutex<Option<Arc<crate::soundevents::SoundEventIndex>>>>,
    /// Where the sound-key check (`crate::tools::sound_check`, review L1) gets its verdicts:
    /// `None` is production, [`crate::tools::sound_check::SoundCheckSource::Index`], the real
    /// soundevent index. The integration tests install `Fixed`/`Unbuildable` through
    /// [`HammerState::set_sound_check_source`] — on the state, not on a thread local, so a tool call
    /// that lands on another runtime thread reads the same override (and a `spawn_blocking` in the
    /// check's path could never lose it).
    pub sound_check_source:
        Arc<Mutex<Option<crate::tools::sound_check::SoundCheckSource>>>,
    /// The soundevents-file gate ([`HammerState::soundevents_gate`], review N2).
    pub soundevents_gate: Arc<AsyncMutex<()>>,
}

impl HammerState {
    pub fn from_config(cfg: &CommonConfig) -> anyhow::Result<Self> {
        let mut path_map = PathMap::for_host(&cfg.project_root, &cfg.app_dir(), &cfg.pfx_dir());
        if let Some(spec) = &cfg.path_map {
            path_map.extend(&PathMap::parse(spec)?);
        }
        Ok(HammerState {
            bridge: BridgeClient::shared(cfg.bridge_options(crate::NAME, crate::VERSION)),
            path_map: Arc::new(path_map),
            edits: Arc::new(Mutex::new(Edits::default())),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            launch_command: cfg.launch_command(),
            expect_build: cfg.expect_build.clone(),
            project_root: cfg.project_root.clone(),
            app_root: cfg.app_dir(),
            runtime: Arc::new(DockerRuntime::new(DockerOptions {
                short_timeout: cfg
                    .podman_short_timeout_ms
                    .map_or(DEFAULT_SHORT_TIMEOUT, std::time::Duration::from_millis),
                image: cfg.image.clone(),
                ..DockerOptions::new(
                    cfg.project_root.clone(),
                    cfg.podman.clone(),
                    cfg.max_sessions,
                )
            })),
            addon_sel: Arc::new(RwLock::new(AddonSel::new(cfg.addon.clone()))),
            tools_container: cfg.tools_container.clone(),
            fgd_root: cfg.app_dir().join("game"),
            gate: Arc::new(AsyncMutex::new(())),
            vtm_gate: Arc::new(AsyncMutex::new(())),
            last_load: Arc::new(Mutex::new(None)),
            pending_vtm_open: Arc::new(Mutex::new(None)),
            compile_jobs: Arc::new(crate::tools::compile_map::CompileJobs::new()),
            model_catalog: Arc::new(Mutex::new(None)),
            soundevent_index: Arc::new(Mutex::new(None)),
            soundevent_index_last: Arc::new(Mutex::new(None)),
            sound_check_source: Arc::new(Mutex::new(None)),
            soundevents_gate: Arc::new(AsyncMutex::new(())),
        })
    }

    /// Drop the cached soundevent index (an addon soundevents file was written or compiled). The
    /// dropped index is parked on [`HammerState::soundevent_index_last`] so the next `index_for`
    /// can rescan the addon without re-decoding the game VPKs (review M3); it is not observable to
    /// a caller (`soundevent_index` is empty, exactly as the name says).
    pub fn soundevent_index_invalidate(&self) {
        if let Ok(mut g) = self.soundevent_index.lock() {
            if let Some(index) = g.take() {
                if let Ok(mut last) = self.soundevent_index_last.lock() {
                    *last = Some(index);
                }
            }
        }
    }

    /// Install the sound-key verdict source (`crate::tools::sound_check::SoundCheckSource`).
    /// **A test seam**: production never calls this (`None`), and `SoundCheckSource::Index` — the
    /// real soundevent index — is what an unset state answers (review L1: the override lives on
    /// the state, not in a thread local, so any task of this server reads it).
    pub fn set_sound_check_source(
        &self,
        source: Option<crate::tools::sound_check::SoundCheckSource>,
    ) {
        *self
            .sound_check_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = source;
    }

    /// The installed verdict source; [`SoundCheckSource::Index`] when nothing was installed.
    pub fn sound_check_source(&self) -> crate::tools::sound_check::SoundCheckSource {
        self.sound_check_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default()
    }

    /// The last document read was untitled (no file behind it).
    pub fn document_untitled(&self) -> bool {
        self.edits.lock().map(|e| e.untitled).unwrap_or(false)
    }

    fn sel(&self) -> std::sync::RwLockReadGuard<'_, AddonSel> {
        self.addon_sel.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The effective addon right now (see [`AddonSel`]).
    pub fn addon(&self) -> String {
        self.sel().current().0
    }

    /// `(addon, source)` of the effective selection.
    pub fn addon_selection(&self) -> (String, AddonSource) {
        self.sel().current()
    }

    /// The path rules for the effective addon ([`HammerState::app_root`] + the addon).
    pub fn content(&self) -> Arc<ContentPaths> {
        Arc::new(ContentPaths::new(self.app_root(), self.addon()))
    }

    /// The game tree root (`--app-root`, default `<project>/workshop/run/app`).
    pub fn app_root(&self) -> PathBuf {
        self.app_root.clone()
    }

    /// Record the addon of the active document (`doc` = a `doc.active` document, `None` = no
    /// active document). A document outside any addon clears the document-derived value.
    pub fn note_document_addon(&self, doc: Option<&Value>) {
        let derived = doc
            .and_then(|d| d.get("path"))
            .and_then(Value::as_str)
            .and_then(|p| self.path_map.windows_to_host(p))
            .and_then(|host| self.content().addon_of(&host));
        let mut sel = self.addon_sel.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        sel.document = derived;
        sel.checked_at = Some(std::time::Instant::now());
        sel.check_failed = false;
        sel.unconfirmed = false;
    }

    /// `set_addon`: `Some(name)` pins an addon, `None` returns to following the document.
    /// Returns the previous `(addon, source)`. The caller validated the name.
    pub fn set_addon(&self, addon: Option<String>) -> (String, AddonSource) {
        let mut sel = self.addon_sel.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = sel.current();
        if addon.is_none() {
            // back to following the document: force the next addon-scoped call to re-read it
            sel.checked_at = None;
        }
        sel.explicit = addon;
        previous
    }

    /// True when the document-derived addon is due for a re-check before a call (no explicit
    /// pin). A `strict` call (anything that changes state) re-checks every time; `throttled`
    /// calls wait for `max_age`; the others re-check every time, except that a failed check
    /// (bridge down) backs off for `max_age` too.
    pub fn addon_recheck_due(&self, max_age: std::time::Duration, throttled: bool, strict: bool) -> bool {
        let sel = self.sel();
        if sel.explicit.is_some() {
            return false;
        }
        if strict {
            return true;
        }
        let backoff = throttled || sel.check_failed;
        sel.checked_at.is_none_or(|t| !backoff || t.elapsed() >= max_age)
    }

    /// The last pre-call check could not confirm the document's addon.
    pub fn addon_unconfirmed(&self) -> bool {
        self.sel().unconfirmed
    }

    /// The pre-call check could not read `doc.active`: keep the last value and back off.
    fn mark_addon_check_failed(&self) {
        let mut sel = self.addon_sel.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        sel.checked_at = Some(std::time::Instant::now());
        sel.check_failed = true;
    }

    /// The pre-call check could not tell (see [`AddonSel::unconfirmed`]).
    fn mark_addon_unconfirmed(&self) {
        self.mark_addon_check_failed();
        self.addon_sel.write().unwrap_or_else(std::sync::PoisonError::into_inner).unconfirmed = true;
    }

    pub fn unsaved_edits(&self) -> u64 {
        self.edits.lock().map(|e| e.since_save).unwrap_or(0)
    }

    /// `"N unsaved bridge edits since last save"` or `None`.
    pub fn unsaved_warning(&self) -> Option<String> {
        match self.unsaved_edits() {
            0 => None,
            n => Some(format!("{n} unsaved bridge edits since last save")),
        }
    }

    /// The server-wide write gate (review-5 L1), locked for **one tool call's whole bridge
    /// sequence**: rmcp runs every request in its own task, and without the gate two concurrent
    /// calls of one server interleave their `doc.active` reads and writes — a mesh rebuild's
    /// `mesh.get` read, its operator and its `mesh.setGeometry` must not be split by another
    /// call's write (the read is of the document as it is NOW, and the write carries the
    /// fingerprint of that very read).
    ///
    /// Held by every **counted write**, from its pre-write `doc.active` read to its reply:
    /// `create_mesh`/`create_block`/`set_mesh_geometry`/`set_mesh_material` and the four rebuild
    /// operators (`mesh_live.rs`), `create_entities`/`set_entity_keyvalues`/`undo_last`
    /// (`entities_live.rs`) and `delete_nodes`/`save_map`/`reload_map`/`open_map`
    /// (`lifecycle.rs`).
    ///
    /// The rules that keep this dead-lock free and simple:
    ///
    /// * one lock per tool call, acquired exactly once at the **top of those handlers** — never
    ///   inside a helper. Every helper (`send_geometry`, `current_mesh`, `require_document`,
    ///   `read_active_document`, `set_all_faces_material`...) runs inside an already-locked
    ///   section and must not lock again (no nested acquisition, no re-entrant waiting).
    /// * held across nothing a person must answer: no call inside the sequence waits on the user
    ///   (the bridge requests each carry a `deadlineMs`, the local work between them is plain
    ///   computation and file reads), so the guard is always dropped by scope exit. The gate
    ///   serializes the tools against each other, never against anything outside this process.
    pub async fn write_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    /// The soundevents-file gate (review N2): what [`crate::tools::soundevents_edit`]
    /// (`edit_soundevents`/`compile_soundevents`) locks instead of [`HammerState::write_gate`].
    ///
    /// A compile runs `resourcecompiler.exe` inside the tools container for up to the compile
    /// timeout — several minutes for a big addon — while touching no bridge state at all, so
    /// holding the server-wide **write** gate for it stopped every bridge mutator for that whole
    /// window (Fable N2). A dedicated gate keeps the file honest among the sound tools — an edit
    /// and a compile of the same `.vsndevts` must not interleave (`write_atomic_with_backup`
    /// would interleave an edit's new text with a compile's `_c` swap, and two concurrent
    /// compiles of one source race for the same `_c`) — while `write_gate` stays free for real
    /// document mutators. One lock per tool call, acquired once at the top of the handler,
    /// exactly like [`HammerState::write_gate`].
    pub async fn soundevents_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.soundevents_gate.lock().await
    }

    /// Reset the counter when the active document changed identity (the POC's `_current_doc`).
    pub fn note_document(&self, doc_id: Option<&str>) {
        if let Ok(mut e) = self.edits.lock() {
            if e.doc_id.as_deref() != doc_id {
                e.doc_id = doc_id.map(str::to_string);
                e.since_save = 0;
            }
        }
    }
}

/// Tools that need no pre-call addon check: they report the selection themselves, never touch a
/// path/catalog/index of the addon, or read `doc.active` themselves before they do
/// (`entities_file::resolve_document`, shared by the file-route reads). Every OTHER tool gets the
/// document-derived addon re-checked before it runs, so a new tool is covered by default: forgetting
/// to list one here costs an extra `doc.active`, never a stale addon.
pub const ADDON_EXEMPT_TOOLS: &[&str] = &[
    // the selection itself and job bookkeeping
    "set_addon", "session_status", "job_status", "job_cancel",
    // live document edits and reads: bridge-only, no addon path/catalog/index
    "undo_last", "delete_nodes", "set_node_transform", "get_selection", "set_selection",
    "create_mesh", "create_block", "set_mesh_geometry", "set_mesh_material", "move_mesh_vertices",
    "extrude_mesh_face", "delete_mesh_faces",
    "add_entity_connections", "remove_entity_connections",
    "create_trigger", "set_trigger_volume", "tie_to_entity", "move_to_world", "list_triggers",
    "render_view", "render_world", "look", "drain_sounds",
    "list_entity_classes", "get_entity_class", "game_state", "input_set", "input_clear", "input_sequence", "play_test",
    "entity_find", "game_rules",
    // the engine console: console_exec edits no document and cvar_get only reads the bridge; the
    // console.log / payload paths hang off the app root, which does not depend on the addon
    "console_exec", "cvar_get",
    // mutators whose handler reads doc.active (`require_document`) before its first addon use
    // (soundevent index, model catalog), after validating its arguments
    "create_entities", "set_entity_keyvalues", "create_props",
    // file-route reads with a `path`: `resolve_document` reads doc.active before resolving it
    "list_meshes", "get_mesh", "transform_mesh", "list_entities", "find_entities", "get_entity", "get_scene_info",
];

/// How old the document-derived addon may be before a bridge-backed read re-reads `doc.active`
/// (those reads refresh it themselves; file tools and writers always re-check).
pub const ADDON_RECHECK: std::time::Duration = std::time::Duration::from_millis(1500);

/// The longest a re-check waits for another call's bridge sequence to finish.
const ADDON_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl ToolState for HammerState {
    fn bridge(&self) -> &SharedBridge {
        &self.bridge
    }

    fn before_call<'a>(
        &'a self,
        spec: &'a ToolSpec,
        args: &'a Value,
        budget: &'a Budget,
    ) -> cs2_mcp_common::tools::BoxFuture<'a, Result<Option<String>, ToolError>> {
        Box::pin(async move {
            if ADDON_EXEMPT_TOOLS.contains(&spec.name) {
                return Ok(None);
            }
            // a poll/cancel of a known job names no path (it must not queue behind the start
            // that holds the bridge)
            if spec.name == "compile_map" && args.get("jobId").is_some() {
                return Ok(None);
            }
            // Throttle only the bridge-backed reads (they read `doc.active` themselves); file
            // tools and every mutator see the current document, whatever happened 1 s ago.
            let throttled = spec.mutability == Mutability::ReadOnly && !spec.bridge_methods.is_empty();
            // A tool that changes anything must not run on an addon it could not confirm; a
            // read-only one proceeds on the last known addon and says so.
            let strict = spec.mutability != Mutability::ReadOnly;
            if !self.addon_recheck_due(ADDON_RECHECK, throttled, strict) {
                return Ok(self.addon_unconfirmed().then(|| {
                    let (addon, source) = self.addon_selection();
                    format!(
                        "the addon could not be confirmed against the active document on the last \
                         check; used {addon:?} (from {})",
                        source.as_str()
                    )
                }));
            }
            let uncertain = |why: String| -> Result<Option<String>, ToolError> {
                self.mark_addon_unconfirmed();
                let (addon, source) = self.addon_selection();
                if strict {
                    Err(addon_check_failed(spec.name, &addon, source, &why))
                } else {
                    Ok(Some(format!(
                        "the addon could not be confirmed against the active document ({why}); \
                         used {addon:?} (from {})",
                        source.as_str()
                    )))
                }
            };
            // wait behind another call's bridge sequence rather than skip the check, but never
            // longer than the tool has left
            let wait = ADDON_LOCK_WAIT.min(budget.remaining());
            let Ok(mut bridge) = tokio::time::timeout(wait, self.bridge.lock()).await else {
                return uncertain(format!(
                    "the bridge stayed busy with another call for {} ms",
                    wait.as_millis()
                ));
            };
            let doc = match bridge.ensure_connected().await {
                Ok(_) => {
                    crate::tools::live_common::read_active_document_locked(self, &mut bridge, Some(1500))
                        .await
                }
                Err(error) => Err(error),
            };
            drop(bridge);
            match doc {
                // read_active_document_locked already noted the document's addon
                Ok(_) => Ok(None),
                // Hammer is not up: there is no document, the addon is the pin or the flag
                Err(e) if matches!(e.kind, BridgeKind::BridgeNotRunning | BridgeKind::HammerNotLoaded) => {
                    self.note_document_addon(None);
                    self.mark_addon_check_failed();
                    Ok(None)
                }
                // Hammer's main thread is stuck (modal dialog, engine busy): no document can change
                // under a stuck editor, so the last known addon is the best answer there is. Every
                // tool proceeds, with the note (run_map must still reach its own EngineBusy answer).
                Err(e) if matches!(e.kind, BridgeKind::HammerBlocked | BridgeKind::EngineBusy) => {
                    self.mark_addon_unconfirmed();
                    let (addon, source) = self.addon_selection();
                    Ok(Some(format!(
                        "the addon could not be confirmed against the active document ({}); used \
                         {addon:?} (from {})",
                        e.message,
                        source.as_str()
                    )))
                }
                Err(e) => uncertain(e.message.clone()),
            }
        })
    }
}

fn addon_check_failed(tool: &str, addon: &str, source: AddonSource, why: &str) -> ToolError {
    ToolError::new(
        "AddonCheckFailed",
        "AddonCheckFailed",
        format!(
            "{tool} was not run: the addon could not be confirmed against the active Hammer \
             document ({why}); the last known addon is {addon:?} (from {}). Nothing was changed; \
             retry the call, or pin the addon with set_addon.",
            source.as_str()
        ),
    )
    .with_code(-32010)
    .with_data(json!({"addon": addon, "addonSource": source.as_str()}))
    .retry_safe(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The document state a `doc.active` reply carries (`DocState::read`): `(id, topSerial,
    /// undoDepth)`, and `None` when the reply cannot be compared (no both undo fields).
    #[test]
    fn doc_state_reads_the_three_numbers_the_rule_compares() {
        assert_eq!(
            DocState::read(&json!({"id": "d1", "topSerial": 4, "undoDepth": 3})),
            Some(DocState { doc_id: Some("d1".into()), serial: 4, depth: 3 })
        );
        assert_eq!(
            DocState::read(&json!({"id": "d1", "topSerial": 0, "undoDepth": 0})),
            Some(DocState { doc_id: Some("d1".into()), serial: 0, depth: 0 })
        );
        // a document with no id is still comparable (the id is `None` on both sides)
        assert_eq!(
            DocState::read(&json!({"topSerial": 2, "undoDepth": 1})),
            Some(DocState { doc_id: None, serial: 2, depth: 1 })
        );
        // `undoDepth` is signed live; a negative one is not a depth, and without both fields there
        // is nothing to compare
        for bad in [
            json!({"id": "d1", "topSerial": 4}),
            json!({"id": "d1", "undoDepth": 3}),
            json!({"id": "d1", "topSerial": 4, "undoDepth": -1}),
            json!({"id": "d1", "topSerial": null, "undoDepth": 3}),
            json!({"id": "d1", "topSerial": 4, "undoDepth": "3"}),
            json!({}),
        ] {
            assert_eq!(DocState::read(&bad), None, "{bad}");
        }
    }

    /// A document switch resets the unsaved counter, the same document never does.
    #[test]
    fn note_document_resets_the_counter_only_on_a_switch() {
        let state = state_for_test();
        state.note_document(Some("d1"));
        state.edits.lock().unwrap().since_save = 3;
        state.note_document(Some("d1"));
        assert_eq!(state.unsaved_edits(), 3, "the same document keeps its count");
        state.note_document(Some("d2"));
        assert_eq!(state.unsaved_edits(), 0, "the identity switch resets the counter");
    }

    /// A `HammerState` with no bridge work (the shared client connects lazily and nothing here
    /// calls it).
    fn state_for_test() -> HammerState {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: CommonConfig,
        }
        let argv: Vec<&str> = vec![
            "t",
            "--project-root",
            "/nonexistent/cs2mcp-state-test",
            "--podman",
            "/nonexistent/cs2mcp-state-test-podman",
            "--vk-layer-dir",
            "/nonexistent/cs2mcp-state-test-vklayer",
            "--xauthority",
            "/nonexistent/cs2mcp-state-test-xauth",
        ];
        HammerState::from_config(&W::parse_from(argv).c).expect("state")
    }
}
