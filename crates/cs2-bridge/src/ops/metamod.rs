//! `metamod.plugins`: the Metamod plugin table, snapshotted from Metamod's
//! `ISmmPluginManager` and answered from the snapshot on the IPC lane.
//!
//! # Why a snapshot
//!
//! The plugin manager (`CPluginManager`, exposed as `ISmmPluginManager` through
//! `MetaFactory("IPluginManager")`, `core/metamod.cpp:814-821`) is **main-thread data**: its
//! plugin list is mutated by `meta load`/`meta unload` on Metamod's thread, and nothing in the
//! `ISmmPluginManager` interface promises any locking. The bridge's IPC thread must therefore
//! never call into it - but it is exactly the thread that answers a status row, and the answer
//! must come even when nothing ticks (the main-thread drain needs a frame). So the table is
//! **rebuilt on the main thread** - where Metamod already serialises us - at the events that
//! change it, published under a [`Mutex`], and the method reads only the snapshot:
//!
//! * once in [`crate::plugin`]'s `Load` (a late `meta load` of this plugin: every other plugin
//!   is already in the table);
//! * at `AllPluginsLoaded` (`core/metamod_plugins.cpp:249` fires `OnPluginLoad` for each plugin
//!   of the initial batch first, so the batch is in the table either way);
//! * at `OnPluginLoad(id)` / `OnPluginUnload(id)` (`:345`: after the change, with the old id);
//! * at `OnPluginPause(id)` / `OnPluginUnpause(id)` (`:303-305`, `:323-325`: Metamod fires them
//!   after `_Pause`/`_Unpause` have already set the row's status, and they carry the id), so a
//!   pause on another plugin is visible on the event.
//!
//! One pause is invisible BY DESIGN - our own: Metamod's `ITER_PLEVENT` skips the acting
//! plugin (`core/metamod_plugins.cpp:95-104`, `if (_Xpl->m_Id == plid) continue;`), so no
//! `OnPluginPause` reaches OUR listener for OUR `meta pause` - and `Pl_Paused` is set after
//! our `Pause` returns (`:713`), so there is no callback of ours to rebuild from either. Our
//! row keeps saying `running` until some other plugin event rebuilds the table; that gap is
//! documented here rather than papered over, because the one honest fix (re-querying the
//! manager) has no main-thread trigger while we are paused.
//!
//! # Forced unloads leave a stale table
//!
//! `unload_body`'s refusal path (an `UnloadTimeout`, say) returns false and leaves the
//! snapshot, the manager pointer and `PRESENT` standing - Metamod still holds the plugin. But
//! Metamod's `_Unload` is `pl->m_API->Unload(...) || force` (`core/metamod_plugins.cpp:656`):
//! `meta clear`, `UnloadAll` and a retry force the unload past a refusal. On the mm+tool
//! image the mapping survives (it is the combined image, never unmapped under the tools
//! host), so the bridge's statics survive too - and the table now says whatever was true
//! BEFORE the forced unload (our row `running` in it, while Metamod's own list no longer has
//! us; the `OnPluginUnload` event we could have learned from is, again, fired to the OTHER
//! plugins only, `:95-104`). The next real `Load` rebuilds; until then the row answers a
//! stale table. This is the documented cost of a forced unload; a plain `meta unload`
//! (refusal honoured) keeps the plugin AND its table consistent instead.
//!
//! # The id bound
//!
//! A rebuild queries ids `Pl_MinId ..= max_seen + 16`, where `max_seen` is the highest id any
//! event has named or this plugin's own id. The bound exists because `m_LastId` - the number a
//! full scan would go up to - is **not reachable through the interface**: `Query` answers per
//! id, `QueryRunning`/`QueryHandle` take an id, and none of them returns the manager's
//! high-water mark. Ids are dense-ish (they increment per load, `core/metamod_plugins.cpp:461-
//! 464`) but can have gaps (unloaded plugins leave holes until a `Retry` reclaims with
//! `m_LastId--`, `:378`, `:390`), so the scan skips every id `Query` answers FALSE for
//! ("not found", `core/IPluginManager.h:~125`) - and only those: a `Query` TRUE with status
//! `Pl_NotFound` (a `meta load typo` entry Metamod keeps in m_Plugins, `:476-483`, shown
//! NOFILE by `meta list`) is a row with status `"not_found"`. A plugin loaded more than 16
//! ids past
//! `max_seen` in one Metamod action cannot happen outside the initial batch (each load fires
//! `OnPluginLoad`, which lifts the bound before the next rebuild); a plugin Metamod loaded
//! through a path that fires no listener event would appear one event later - a documented
//! staleness, never a wrong row.
//!
//! # Ownership
//!
//! `Query`'s `file` answer is Metamod's pointer (`core/IPluginManager.h`'s "Pointer to store
//! filename pointer"): the rebuild copies the bytes into an owned [`String`] immediately and
//! keeps nothing borrowed. Status names come from the generated `Pl_*` consts; a value the
//! generator does not name is reported as `unknown(<n>)` rather than guessed.
//!
//! # The row and the gate
//!
//! `metamod.plugins` is answered on the IPC lane from the snapshot (see [`crate::wiring`]'s
//! `route`), so it carries `needs == 0` in [`crate::registry`] `RUST_METHODS` - a property of
//! THIS row (a lane-answered row must never be queued behind the tick), not of being a Rust
//! row. It is deliberately NOT in an `ops` `HANDLERS` table - those are the main-thread
//! bodies (`ops::mod`'s own test pins every handler's row to `needs != 0`), and queueing this
//! row would put the answer behind a tick. Because the lane route bypasses the gate, the two
//! metamod prerequisites come from the row itself, as `-32017 PrerequisiteFailed` with a
//! `blockedOn` ([`METAMOD_BLOCKED_ON`][crate::registry::METAMOD_BLOCKED_ON] is the "not
//! loaded" one's text):
//!
//! * Metamod absent ([`crate::plugin::is_present`] false): `"metamod: not loaded"`;
//! * `Load` completed but the manager was not handed out / had no `MetaFactory` slot:
//!   `"metamod: IPluginManager unavailable"` (`Load` still succeeds - the table is a
//!   status read, not a dependency).

use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::ffi::CString;
use std::sync::{LazyLock, Mutex, MutexGuard};

use s2sdk_sys::generated::{ISmmAPI, ISmmPluginManager, MMIFACE_PLMANAGER, Pl_Status};

use crate::control;
use crate::entity_json::Sb;
use crate::log;

/// The method this module serves on the control plane.
pub const METHOD: &str = "metamod.plugins";

/// The first id Metamod hands out (`core/IPluginManager.h:48`'s `Pl_MinId`), measured into
/// [`s2sdk_sys::generated`] by the generator's consts route (the enumerators of that unnamed
/// enum are namespace-scope names, so `SourceMM::Pl_MinId` is an expression clang evaluates -
/// no hand-written constant here, and no hand edit of the generated file).
pub use s2sdk_sys::generated::PL_MIN_ID;

/// How far past the highest id seen a rebuild scans. See the module docs ("The id bound"):
/// `m_LastId` is not reachable through the interface, and no Metamod path can open a gap
/// wider than this between two events this listener sees.
const ID_HEADROOM: i32 = 16;

/// The `blockedOn` of a `Load` that completed without an `ISmmPluginManager` (the generated
/// `MMIFACE_PLMANAGER` query refused, or the API has no `MetaFactory` slot). `Load` still
/// succeeds; the row answers this until a re-`Load`.
pub const PLMGR_BLOCKED_ON: &str = "metamod: IPluginManager unavailable";

/// `-32017 PrerequisiteFailed`'s code and message, the shapes
/// [`crate::control::khook_missing`] answers the other non-module prerequisite with.
const PREREQUISITE_CODE: i64 = -32017;
const PREREQUISITE_MESSAGE: &str = "PrerequisiteFailed";

/// The `ISmmPluginManager*` `Load` fetched through `MetaFactory` (the manager Metamod serves
/// for [`MMIFACE_PLMANAGER`]). Borrowed, never freed: Metamod owns it (`core/metamod.cpp:820`
/// answers `&g_PluginMngr`, a static). Cleared in `Unload`'s teardown - after Metamod's
/// `Unload` returns, this plugin's right to touch Metamod's state is gone.
static PLUGIN_MANAGER: AtomicPtr<ISmmPluginManager> = AtomicPtr::new(ptr::null_mut());

/// The highest plugin id any `OnPluginLoad`/`OnPluginUnload` event has named, or this
/// plugin's own id - the base of the rebuild's scan bound.
static MAX_SEEN_ID: AtomicI32 = AtomicI32::new(0);

/// [`MMIFACE_PLMANAGER`] as the NUL-terminated name `MetaFactory` compares
/// (`core/metamod.cpp:813`, a plain `strcmp`).
static PLMANAGER_NAME: LazyLock<CString> =
    LazyLock::new(|| CString::new(MMIFACE_PLMANAGER).expect("the generated name has no NUL"));

/// One row of the snapshot: one Metamod plugin as `ISmmPluginManager::Query` answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// The `PluginId` (`Pl_MinId` upwards).
    pub id: i32,
    /// The plugin file, copied out of Metamod's string at rebuild time (owned here; Metamod
    /// owns the pointer and may reuse it after the call).
    pub file: String,
    /// The `Pl_Status` as a name; the generated consts' values, or `unknown(<n>)`.
    pub status: String,
    /// The `PluginId` of the loading source (`core/IPluginManager.h:44-49`): `Pl_File`
    /// (-2) for a tools-vdf/ini load (`core/metamod.cpp:326`, `:1182`), `Pl_Console` (-1)
    /// for a console `meta load` (`core/metamod_console.cpp:428`), `Pl_BadLoad` (0) when the
    /// load itself failed.
    pub source: i32,
    /// Whether this row is this plugin (`id == crate::plugin::plugin_id()`).
    pub is_self: bool,
}

/// One published table: the rows, how many rebuilds produced them, and when the last one ran
/// ([`crate::wiring::mono_ms`], the same clock the control replies use). Cloned out for the
/// tests and probes; the wire answer is [`plugins_answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// One row per plugin the manager found, ids ascending.
    pub rows: Vec<PluginRow>,
    /// `+1` per rebuild that had a manager to query.
    pub generation: u64,
    /// When the last rebuild ran, on [`crate::wiring::mono_ms`].
    pub rebuilt_at_ms: u64,
}

/// The published table. Written on the main thread inside Metamod's callbacks, read on the
/// IPC threads - the lock is the whole handshake, and no `MutexGuard` leaves [`snapshot`].
static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

fn snapshot() -> MutexGuard<'static, Option<Snapshot>> {
    // No invariant spans a lock in this module (rebuild replaces the whole slot), so
    // poisoning cannot leave one half-updated: take the recovered guard.
    SNAPSHOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The manager pointer `Load` stored, or null.
#[must_use]
pub fn plugin_manager() -> *mut ISmmPluginManager {
    PLUGIN_MANAGER.load(Ordering::Acquire)
}

/// `Load`'s extra step: fetch the `ISmmPluginManager` through the API's `MetaFactory`
/// (`core/metamod.cpp:814-821` serves [`MMIFACE_PLMANAGER`]). A missing slot, a refused
/// answer or a null pointer is logged and leaves the table unavailable - **`Load` still
/// succeeds** (the brief: the table is a status read, not a dependency). Runs on the main
/// thread, inside Metamod's `Load`.
///
/// # Safety
///
/// `ismm` must be the live `ISmmAPI` Metamod handed `Load` (the pointer the generated
/// `ISmmAPI::vtable` dereferences), on the main thread.
pub unsafe fn fetch_plugin_manager(ismm: *mut ISmmAPI) {
    // Forget whatever an earlier load in this image stored: one live manager at a time.
    PLUGIN_MANAGER.store(ptr::null_mut(), Ordering::Release);
    // SAFETY: `ismm` is the live `ISmmAPI` Metamod handed `Load` (`core/ISmmPlugin.h:107`),
    // and its vtable pointer is the object's own - `plugin::load` has already read this far.
    let api = unsafe { ISmmAPI::vtable(ismm) };
    let Some(meta_factory) = api.MetaFactory else {
        log::line("metamod: ISmmAPI has no MetaFactory slot; IPluginManager unavailable");
        return;
    };
    let mut ret: c_int = 0;
    // SAFETY: the name is a valid NUL-terminated literal, `ret` outlives the call, and the
    // id out-pointer is null - Metamod's MetaFactory tests every pointer (`if (ret)`,
    // `core/metamod.cpp:816`).
    let manager = unsafe {
        meta_factory(
            ismm,
            PLMANAGER_NAME.as_ptr(),
            &mut ret as *mut c_int,
            ptr::null_mut(),
        )
    };
    if manager.is_null() || ret != crate::plugin::META_IFACE_OK {
        log::line(&format!(
            "metamod: MetaFactory(\"{MMIFACE_PLMANAGER}\") answered ret={ret} ptr={} \
             (wanted ret=META_IFACE_OK); IPluginManager unavailable",
            if manager.is_null() { "null" } else { "set" }
        ));
        return;
    }
    PLUGIN_MANAGER.store(manager.cast::<ISmmPluginManager>(), Ordering::Release);
    log::line("metamod: IPluginManager available");
}

/// Record a plugin id an `OnPluginLoad`/`OnPluginUnload` event named: the scan bound's base.
pub fn note_event_id(id: c_int) {
    MAX_SEEN_ID.fetch_max(id, Ordering::Release);
}

/// Rebuild the snapshot from the manager: `Query` for every id in
/// [`PL_MIN_ID`]`..=max_seen + 16` (see the module docs for the bound), skipping every id the
/// manager answers "not found" for. Main thread only - it calls into Metamod's plugin state.
///
/// A rebuild with no manager (the never-loaded or unavailable case) leaves the snapshot as it
/// was: the row answers `blockedOn` for that state instead of an empty table.
pub fn rebuild() {
    let manager = PLUGIN_MANAGER.load(Ordering::Acquire);
    if manager.is_null() {
        return;
    }
    // SAFETY: `manager` is Metamod's live `ISmmPluginManager` (stored at `Load`, forgotten in
    // `Unload`), and this runs on the main thread - the only thread that may ask it.
    let vt = unsafe { ISmmPluginManager::vtable(manager) };
    let Some(query) = vt.Query else {
        log::line("metamod: ISmmPluginManager has no Query slot; the table is unavailable");
        return;
    };

    let own_id = crate::plugin::plugin_id();
    let max_seen = MAX_SEEN_ID.load(Ordering::Acquire).max(own_id).max(PL_MIN_ID);
    let last = max_seen.saturating_add(ID_HEADROOM);

    let mut rows = Vec::new();
    for id in PL_MIN_ID..=last {
        let mut file: *const c_char = ptr::null();
        let mut status = Pl_Status(Pl_Status::Pl_NotFound.0);
        let mut source: c_int = 0;
        // SAFETY: the three out-pointers are valid for the call, and the `file` answer is
        // Metamod's own string, copied out immediately below (never stored).
        let found = unsafe { query(manager, id, &mut file, &mut status, &mut source) };
        if !found {
            // `Query` FALSE is the table's only gap: the id names no plugin
            // (`core/IPluginManager.h:~125`'s "false if not found"; `FindById` missed,
            // `core/metamod_plugins.cpp:768-775`).
            continue;
        }
        // A `Query` TRUE with `Pl_NotFound` is a ROW, not the gap: a `meta load typo`
        // leaves its entry in m_Plugins with that status (`core/metamod_plugins.cpp:476-483`),
        // and `meta list` shows it as NOFILE (`GetStatusText`, `:426-428`). Hiding it here
        // would hide a plugin the operator tried to load, so the row is published with
        // `status: "not_found"`.
        let file = unsafe { copy_file(file) };
        rows.push(PluginRow {
            id,
            file,
            status: status_name(status),
            source,
            is_self: id == own_id,
        });
    }

    let mut slot = snapshot();
    let generation = slot.as_ref().map_or(0, |snap| snap.generation) + 1;
    *slot = Some(Snapshot {
        rows,
        generation,
        rebuilt_at_ms: crate::wiring::mono_ms(),
    });
    drop(slot);
    log::line("metamod: plugin table rebuilt");
}

/// Copy Metamod's `file` answer into an owned [`String`]. A null answer (the header allows
/// ignoring the out-pointer) is the empty string.
unsafe fn copy_file(file: *const c_char) -> String {
    if file.is_null() {
        return String::new();
    }
    // SAFETY: the caller's contract - a NUL-terminated string Metamod owns for the duration
    // of the `Query` call; the copy is made before returning.
    unsafe { core::ffi::CStr::from_ptr(file) }
        .to_string_lossy()
        .into_owned()
}

/// The generated `Pl_*` values as names; anything else is reported, not guessed.
fn status_name(status: Pl_Status) -> String {
    match status.0 {
        s if s == Pl_Status::Pl_Running.0 => "running".to_string(),
        s if s == Pl_Status::Pl_Paused.0 => "paused".to_string(),
        s if s == Pl_Status::Pl_Refused.0 => "refused".to_string(),
        s if s == Pl_Status::Pl_Error.0 => "error".to_string(),
        s if s == Pl_Status::Pl_NotFound.0 => "not_found".to_string(),
        other => format!("unknown({other})"),
    }
}

/// `Unload`'s teardown: forget the manager (Metamod's state is no longer ours to touch once
/// its `Unload` call returns) and drop the snapshot (a stale table must never be served as
/// current). Main thread, in `plugin::unload_body`'s tail.
pub fn forget() {
    PLUGIN_MANAGER.store(ptr::null_mut(), Ordering::Release);
    MAX_SEEN_ID.store(0, Ordering::Release);
    *snapshot() = None;
}

/// Whether this module serves `name` on the control plane.
#[must_use]
pub fn serves(name: &str) -> bool {
    name == METHOD
}

/// The control-plane answer for [`METHOD`]: `Ok` is the bare result object (the lane's
/// envelope wraps it, as for `control.*`), `Err` is `(code, message, data_json)` for a
/// `-32017 PrerequisiteFailed`. Never touches the manager or the queue: the snapshot under
/// the [`Mutex`] is the only state read, so the answer works when nothing ticks.
///
/// Metamod absent -> `"metamod: not loaded"`; present but the manager unavailable (see
/// [`fetch_plugin_manager`]) -> `"metamod: IPluginManager unavailable"`.
pub fn plugins_answer() -> Result<String, (i64, &'static str, String)> {
    if !crate::plugin::is_present() {
        return Err((
            PREREQUISITE_CODE,
            PREREQUISITE_MESSAGE,
            control::blocked_on_data(crate::registry::METAMOD_BLOCKED_ON),
        ));
    }
    if PLUGIN_MANAGER.load(Ordering::Acquire).is_null() {
        return Err((PREREQUISITE_CODE, PREREQUISITE_MESSAGE, control::blocked_on_data(PLMGR_BLOCKED_ON)));
    }

    let slot = snapshot();
    let Some(snap) = slot.as_ref() else {
        // Reachable: an IPC read racing `Load` can see the manager stored but the snapshot
        // not yet published (`Load` stores the pointer, then rebuilds) - a brief window,
        // answered honestly as the empty table with `generation: 0` rather than a guess;
        // the next rebuild replaces it. (`Unload`'s `forget()` clears the manager FIRST,
        // so a read racing the teardown answers the `IPluginManager unavailable` guard
        // instead, never this arm.)
        return Ok("{\"plugins\":[],\"generation\":0,\"rebuiltAtMs\":0}".to_string());
    };

    // The result object, in the C's reply-buffer habit (`control::MAX_LINE`): rows are ids,
    // statuses, sources and file paths, but the buffer is the same one every control reply
    // builds in, so an absurd table truncates exactly where the C's would.
    let mut buf = vec![0u8; control::MAX_LINE as usize];
    let mut sb = Sb::new(&mut buf);
    sb.add(b"{\"plugins\":[");
    for (i, row) in snap.rows.iter().enumerate() {
        if i != 0 {
            sb.add(b",");
        }
        sb.add(b"{\"id\":");
        sb.addi(i64::from(row.id));
        sb.add(b",\"file\":");
        sb.addjson(row.file.as_bytes());
        sb.add(b",\"status\":");
        sb.addjson(row.status.as_bytes());
        sb.add(b",\"source\":");
        sb.addi(i64::from(row.source));
        sb.add(b",\"self\":");
        sb.add(if row.is_self { b"true" } else { b"false" });
        sb.add(b"}");
    }
    sb.add(b"],\"generation\":");
    sb.addnum(snap.generation);
    sb.add(b",\"rebuiltAtMs\":");
    sb.addnum(snap.rebuilt_at_ms);
    sb.add(b"}");
    if sb.ovf() {
        // A truncated result would be a corrupt JSON frame; the C's dispatch tail answers
        // `-32603` when a result builder overflows (`ops::finish`'s overflow arm).
        return Err((
            -32603,
            "InternalError",
            "{\"reason\":\"plugin table too large\"}".to_string(),
        ));
    }
    // `addjson` escapes every string it writes; the lossy conversion is the identity except
    // on an overflow-truncated multibyte sequence (handled above).
    Ok(String::from_utf8_lossy(sb.as_cstr()).into_owned())
}

/// The published table, cloned: what [`plugins_answer`] renders (and what the tests pin).
/// `None` = no rebuild has run (Metamod never loaded, or the manager is unavailable).
#[must_use]
pub fn current_snapshot() -> Option<Snapshot> {
    snapshot().clone()
}

// The registry row lives in `crate::registry` `RUST_METHODS` (the frozen C table cannot
// carry a Rust-only row); this module owns the answer (`plugins_answer`), which
// `crate::wiring`'s lane route serves.
