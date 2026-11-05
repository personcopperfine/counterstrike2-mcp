//! The Metamod:Source 2.0-dev plugin: `CreateInterface("ISmmPlugin")` and the `Load`/`Unload`
//! lifecycle around it.
//!
//! Metamod finds a plugin by `CreateInterface(METAMOD_PLAPI_NAME, NULL)` and asks the object it
//! gets back for `GetApiVersion`, then calls `Load` (`core/metamod_plugins.cpp:569-620`). A C++
//! plugin gets that object from the `PLUGIN_EXPOSE`/`PLUGIN_SAVEVARS` macros
//! (`core/ISmmPlugin.h:490-518`), whose whole job is:
//!
//! ```text
//! EXPOSE_SINGLE_INTERFACE(ISmmPlugin, METAMOD_PLAPI_NAME, <the singleton>)
//! Load():  g_SMAPI = ismm;
//!          KHook::__exported__khook = ismm->GetDetourInterface(id);
//!          g_PLAPI = this;  g_PLID = id;
//! ```
//!
//! This module does exactly that, with the generated [`ISmmPlugin_vtable`] as the object's
//! table and [`khook_api`](crate::khook_api) as the `IKHook` adapter. It also registers an
//! [`IMetamodListener`] whose `OnMetamodQuery` answers our own `McpBridge_001` object
//! ([`iface`](crate::iface)), the way a C++ plugin registers its listener with
//! `g_SMAPI->AddListener(this, this)` (`samples/s2_sample_mm/src/plugin.cpp:96`).
//!
//! # Two lifecycles in one DLL
//!
//! The tool plugin (`ToolSystem2_001`, Windows only) needs no Metamod: the tools framework
//! creates that object and drives it every frame, and that is how `-tools` runs today. Metamod
//! presence is therefore an explicit state ([`is_present`]) rather than an assumption, and the
//! `McpBridge_001` status line reports it as `"metamod":"absent"` until a `Load` succeeds.
//!
//! # One image, two roles
//!
//! The Wine tools' Metamod vdf points at `../bin/win64/tools/cs2_bridge.dll`, so the Metamod
//! plugin and the tools framework load the **same file**: one mapping, one set of statics, one
//! log. Metamod's `Load` runs first (at server-lib init), then the tools framework's
//! `CreateInterface("ToolSystem2_001")`, `Connect`, `Init` and the per-frame ticks. On the
//! Linux client and dedicated server there is only the Metamod role.
//!
//! So this image has a role **set** (`log::Role`), not one name: `mm` from
//! `CreateInterface("ISmmPlugin")`/`Load`, `tool` from the tools object. Every state this
//! module and [`iface`] report is that one image's - "Metamod present" means a `Load`
//! completed in it - and each log line carries the set (`[cs2-bridge mm]`,
//! `[cs2-bridge tool]`, `[cs2-bridge mm+tool]`). An earlier design had two DLL copies under two
//! paths with a separate log per copy; that is obsolete and would silently split the tick, the
//! hooks and the state between two images.
//!
//! A Metamod unload only tears down the Metamod role's half: the tool object keeps running. The
//! three rows that install a KHook hook (`game.saveRestore`, `input.set`, `sound.subscribe`)
//! are gated through `NEED_KHOOK`, so they are refused, not mis-run, while Metamod is absent;
//! their off-ramps (`input.clear`/`status`/`uninstall`, `sound.status`/`drain`/`uninstall`) keep
//! answering. Only `input.set` and `sound.subscribe` install as an op runs; the sound report
//! rows arm the wrap only while the API is present and otherwise answer a `blockedOn` note, so
//! they never refuse after `meta unload` (see `registry`'s `NEED_KHOOK` docs).
//!
//! # One object, two CPlugins: `meta retry`
//!
//! `CPluginManager::Retry` (`core/metamod_plugins.cpp:351-398`) is reached from `meta retry <id>`
//! (`core/metamod_console.cpp:604`; its own help text calls it "Attempt to reload a plugin",
//! `:637`) and from nothing else. It runs only for a plugin whose status is below `Pl_Paused`
//! (`:359-363`, `core/IPluginManager.h:54-61`: NOTFOUND/ERROR/REFUSED), i.e. for a plugin whose
//! `Load` returned false - `Pl_Refused` (`:629`). For that plugin it:
//!
//! 1. calls `_Load` on the same file (`:364`), which is `dlmount` = `dlopen`/`LoadLibrary` on the
//!    path the old `CPlugin` already has open (`core/metamod_oslink.h:43`, `:66`), so the loader
//!    refcounts: the same image, and therefore the **same static `ISmmPlugin` object**, is what
//!    Metamod's second `CPlugin` resolves (`:570`) and `Load`s (`:619`);
//! 2. when that load succeeded, calls `_Unload` on the *old* `CPlugin` with `force = true`
//!    (`:372`, "Now it gets crazy..."), which calls `Unload` on that same object (`:656`) and
//!    then deletes the old `CPlugin`;
//! 3. renames the new `CPlugin` to the old id (`:375`).
//!
//! So this plugin's own `Unload` is called for a `CPlugin` that is *not* the one this object is
//! live under: at that moment [`is_present`] is true because the **new** load just completed, and
//! the call tears the new copy's state down - KHook uninstalled, listener flag cleared,
//! `"metamod":"absent"` - while Metamod keeps the new `CPlugin` as `Pl_Running`.
//!
//! ## What was decided, and why nothing is guarded
//!
//! The call is torn down as asked, and the mismatch is documented rather than papered over,
//! because the plugin cannot tell the two callers apart - the evidence, from Metamod's source:
//!
//! * `ISmmPlugin::Unload(error, maxlen)` (`core/ISmmPlugin.h:154`) carries no id, instance or
//!   handle: the object itself is the only identity a callee is given, and here both `CPlugin`s
//!   have the same one (`object()` is a static, so a second `Load` cannot even be given a
//!   different `this`).
//! * Nor can the caller be identified from the other side. The only Metamod-side way to ask
//!   "which `CPlugin`s hold this `ISmmPlugin*`" is `CPluginManager::FindByAPI`
//!   (`core/metamod_plugins.cpp:400-419`), which is **not** part of the interface a plugin can
//!   reach: `CPluginManager` is exposed as `ISmmPluginManager`
//!   (`core/IPluginManager.h:66-150`, served by `MetamodSource::MetaFactory` for
//!   `"IPluginManager"`, `core/metamod.cpp:814-821`, `core/ISmmAPI.h:212`), whose `Query`,
//!   `QueryRunning` and `QueryHandle` all take an id and none of them answers an
//!   `ISmmPlugin*`. Ids are not identities either: `Retry` reclaims `m_LastId--` (`:378`, `:390`)
//!   and rewrites the new `CPlugin`'s id (`:375`), so an id remembered from an earlier call can
//!   name a different `CPlugin` later.
//! * A "load generation" counter cannot help for the same reason: `Load` and `Unload` are not
//!   paired with anything the callee can compare. In the retry sequence the previous `Load` for
//!   the *old* copy is the one that *failed*, so it incremented nothing, and the new copy's
//!   `Load` is followed by exactly one `Unload` - indistinguishable from `meta unload` of a
//!   running plugin, which must tear down.
//!
//! What the guard would have to decide is therefore not decidable from inside this image, and a
//! guess would be worse than the mismatch: refusing a real `Unload` leaves hooks live past the
//! image's unmap, which is the one failure this plugin's teardown exists to prevent.
//!
//! ## What the state is after it, and how it recovers
//!
//! * Memory-safe, because `Unload` drains through the `IKHook` the **new** load installed: every
//!   hook the registry can hold was registered after that install, and the old `CPlugin`'s
//!   `KHookImpl` is released by that `CPlugin`'s own destructor
//!   (`CPluginManager::CPlugin::~CPlugin`, `core/metamod_plugins.cpp:205-214`). The old copy's
//!   history here is a *refused* `Load`, and a refused `Load` installs no adapter and registers no
//!   hook - every refusal path returns before [`khook_api::install`].
//! * Metamod's new `CPlugin` is `Pl_Running` while this object reports `"metamod":"absent"` and
//!   `QueryRunning` answers false (`core/ISmmPlugin.h:141`'s "true for okay, false for badness"),
//!   so Metamod's own view of the plugin and this plugin's view of itself disagree until the
//!   operator acts. `Load` also refuses a second call while [`is_present`] (see there).
//! * The recovery is a fresh load, not a re-`Load` in the same image: `meta unload` on the
//!   renamed `CPlugin` deletes it, which `dlclose`s this image once the last reference is gone
//!   (`CPluginManager::CPlugin::~CPlugin`, `core/metamod_plugins.cpp:205-214`, `:71`) - the
//!   statics go with it - and the next `meta load` is a clean start. The object itself is
//!   reusable in the meantime, which `tests/metamod_plugin.rs` drives: a retry sequence in
//!   `retry_loads_the_same_object_and_the_superseded_unload_tears_the_new_copy_down`, then a
//!   `Load` that succeeds.
//!
//! # Teardown
//!
//! `Unload` tears down exactly what `Load` started and nothing else. The tool object's shutdown
//! sequence (`mcp_bridge.c:6763-6795`: `m0_Shutdown` -> `m0_PreShutdown` -> "Shutdown
//! done") belongs to the tool object's own `Shutdown` slot, where the tools framework calls
//! it - not to the Metamod lifecycle. One image can serve both roles, so `Unload` never calls
//! `toolsys::shutdown_body`, and the tool half keeps running after a Metamod unload (the row
//! that installs a KHook hook, `game.saveRestore`, answers the `NEED_KHOOK` prerequisite until
//! Metamod is loaded again; the vtable-swap input and sound rows keep serving their off-ramps).
//!
//! What `Unload` does do, in order ([`crate::hooks::unload_hooks`]):
//!
//! 1. every hook owner's own uninstall ([`crate::hooks::hooks_teardown`]), so the feature state
//!    is cleared with its hook and nothing can re-arm one in between;
//! 2. the module verifier is waited out ([`crate::modules::Modules::wait_verifier`], bounded by
//!    [`DEFAULT_UNLOAD_DEADLINE`]). Its identity check asks `s2sdk::hook::api` for a
//!    `&'static dyn KHookApi` (`modules::verify_one`), and Metamod deletes the `KHookImpl` when
//!    `Unload` returns true, so forgetting the API under a running verifier is a use-after-free.
//!    No new verifier can start during the wait: the only starter is
//!    `modules::poll`, called from the tools tick, and Metamod's `Unload` comes from `meta
//!    unload`/`UnloadAll` on the same main thread (see `crate::hooks`' module docs);
//! 3. [`s2sdk::hook::remove_all_blocking`] as the backstop, so **no hook is still reachable
//!    from KHook** when the image goes away; a timeout refuses the unload (`UnloadTimeout`, see
//!    [`DEFAULT_UNLOAD_DEADLINE`]) instead of unloading into a dangling trampoline;
//! 4. [`crate::khook_api::uninstall`], only once that answered `Ok` (or with
//!    `KHookNotInstalled` - nothing left to drain with). Metamod deletes the plugin's
//!    `KHookImpl` in `~CPlugin` (`core/metamod_plugins.cpp:59`), so the `IKHook*` dangles the
//!    moment `Unload` returns true: the adapter has to be forgotten before that.
//!
//! The tail of `unload_body` then forgets `Load`'s remaining state - the listener flag, the
//! `ISmmAPI*`, the id, the present flag, and the plugin-manager state [`crate::ops::metamod`]
//! fetched and snapshotted (`metamod::forget`).
//!
//! A verifier that does not stop in time, and any drain error other than `KHookNotInstalled`,
//! keeps the API and makes `Unload` return false with the message - see the forced-unload
//! section below for why that is the survivable answer.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use core::time::Duration;

use s2sdk_sys::generated::{
    IMetamodListener, IMetamodListener_vtable, ISmmAPI, ISmmPlugin, ISmmPlugin_slots,
    ISmmPlugin_vtable, METAMOD_PLAPI_VERSION,
};

use crate::iface;
use crate::khook_api;
use crate::log;

/// The `META_IFACE_OK` of `core/ISmmPlugin.h:54`: what a listener that answered a query writes
/// into `ret`. Binary-compatible with HL2SDK's `IFACE_OK`.
pub const META_IFACE_OK: c_int = 0;
/// The `META_IFACE_FAILED` of `core/ISmmPlugin.h:55` (the enum's next value).
pub const META_IFACE_FAILED: c_int = 1;

/// How long `Unload` waits for KHook to run the removal functions of this plugin's hooks before
/// refusing the unload, and for the module verifier to finish before it is joined.
///
/// KHook's own `RemoveHook` is synchronous when asked (`detour.cpp:1581-1597`), so the wait is
/// normally the length of a few removal functions; the deadline only bounds the pathological
/// case [`s2sdk::hook::remove_all_blocking`] documents (a hook KHook lost track of). Five
/// seconds is far longer than a healthy teardown and far shorter than a `meta unload` operator's
/// patience. The same value bounds [`crate::modules::Modules::wait_verifier`]: a verifier thread
/// the GameFrame tick started must finish before the image is unmapped, and a verifier that
/// outlives the deadline refuses the unload. [`set_unload_deadline`] exists so a probe or a test
/// can bound both waits.
pub const DEFAULT_UNLOAD_DEADLINE: Duration = Duration::from_secs(5);

/// The deadline [`Unload`] uses for the hook drain and the verifier wait; see
/// [`DEFAULT_UNLOAD_DEADLINE`].
static UNLOAD_DEADLINE_MS: AtomicI32 = AtomicI32::new(5_000);

/// Set the deadline [`Unload`] gives [`s2sdk::hook::remove_all_blocking`] and the module
/// verifier's wait/join ([`crate::modules::Modules::wait_verifier`]).
///
/// For probes and tests only: a plugin sets nothing here. Zero means "do not wait", which is
/// only useful when there is nothing to remove.
pub fn set_unload_deadline(deadline: Duration) {
    let ms = i32::try_from(deadline.as_millis()).unwrap_or(i32::MAX);
    UNLOAD_DEADLINE_MS.store(ms, Ordering::Release);
}

/// The deadline [`Unload`] currently uses.
#[must_use]
pub fn unload_deadline() -> Duration {
    Duration::from_millis(UNLOAD_DEADLINE_MS.load(Ordering::Acquire).max(0) as u64)
}

/// Whether a `Load` has completed and no `Unload` has since succeeded: the `"present"` of the
/// `McpBridge_001` status line.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// The `ISmmAPI*` Metamod passed to `Load` (`PLUGIN_SAVEVARS`'s `g_SMAPI`). Borrowed, never
/// freed (`core/ISmmPlugin.h:107` hands the plugin a pointer Metamod owns).
static ISMM: AtomicPtr<ISmmAPI> = AtomicPtr::new(ptr::null_mut());

/// The `PluginId` Metamod passed to `Load` (`g_PLID`).
static PLUGIN_ID: AtomicI32 = AtomicI32::new(0);

/// Whether [`LISTENER`] has been handed to Metamod; Metamod keeps it until the plugin unloads
/// (there is no `RemoveListener`: `ISmmAPI.h:201` is the only listener call, and
/// `core/metamod_plugins.cpp:658` clears a plugin's listener list when it unloads it).
static LISTENER_REGISTERED: AtomicBool = AtomicBool::new(false);

/// One vtable entry: `void *`, like the C bridge's arrays - the slots have different signatures
/// and the caller calls them by index.
pub type VtEntry = *const c_void;

/// The plugin object's vtable length: the generated per-ABI `VTLEN` (17 on Itanium, where the
/// destructor takes two slots, 16 on MSVC, where it takes one).
pub const VT_LEN: usize = ISmmPlugin_slots::VTLEN;

/// The vtable array, built at compile time from the generated slots.
struct Vtable([VtEntry; VT_LEN]);

// SAFETY: built by the `const` evaluation of `build_vtable`, never written afterwards; the
// entries are read-only function addresses.
unsafe impl Sync for Vtable {}

static VTABLE: Vtable = Vtable(build_vtable());

// The generated struct and the slot constants are two views of one layout: this pins the array
// `build_vtable` fills to the generated struct's own field offsets and size, per ABI (Itanium's
// 17 slots with two destructor entries against MSVC's 16 with one), so a generator change that
// moved a slot would fail this crate's build rather than the game.
const _: () = {
    let word = core::mem::size_of::<VtEntry>();
    assert!(core::mem::offset_of!(ISmmPlugin_vtable, GetApiVersion) == ISmmPlugin_slots::GetApiVersion * word);
    assert!(core::mem::offset_of!(ISmmPlugin_vtable, Load) == ISmmPlugin_slots::Load * word);
    assert!(core::mem::offset_of!(ISmmPlugin_vtable, Unload) == ISmmPlugin_slots::Unload * word);
    assert!(core::mem::offset_of!(ISmmPlugin_vtable, GetLogTag) == ISmmPlugin_slots::GetLogTag * word);
    assert!(core::mem::size_of::<ISmmPlugin_vtable>() == ISmmPlugin_slots::VTLEN * word);
    assert!(core::mem::size_of::<ISmmPlugin_vtable>() == VT_LEN * word);
};

/// The plugin object (`ISmmPlugin` is one vtable pointer; the generated struct is that shape).
struct PluginCell(ISmmPlugin);

// SAFETY: the object is never written after the static initialiser - the generated `ISmmPlugin`
// holds only the vtable pointer, which points at the read-only `VTABLE`.
unsafe impl Sync for PluginCell {}

static PLUGIN: PluginCell = PluginCell(ISmmPlugin {
    vtable: core::ptr::addr_of!(VTABLE.0).cast::<c_void>(),
});

/// The listener object: one vtable pointer, like [`PLUGIN`].
struct ListenerCell(IMetamodListener);

// SAFETY: as for `PluginCell`; the listener has no mutable state of its own (every callback
// reads the process-wide atomics above).
unsafe impl Sync for ListenerCell {}

static LISTENER: ListenerCell = ListenerCell(IMetamodListener {
    vtable: core::ptr::addr_of!(LISTENER_VTABLE).cast::<c_void>(),
});

/// The listener's vtable: every virtual of `IMetamodListener` (`core/ISmmPlugin.h:211-456`),
/// each one at the generated slot. Nothing is null: Metamod calls a listener slot without a
/// null check.
static LISTENER_VTABLE: IMetamodListener_vtable = IMetamodListener_vtable {
    OnPluginLoad: Some(on_plugin_load),
    OnPluginUnload: Some(on_plugin_unload),
    OnPluginPause: Some(on_plugin_pause),
    OnPluginUnpause: Some(on_plugin_unpause),
    OnLevelInit: Some(on_level_init),
    OnLevelShutdown: Some(on_level_shutdown),
    OnEngineQuery: Some(on_engine_query),
    OnPhysicsQuery: Some(on_physics_query),
    OnFileSystemQuery: Some(on_file_system_query),
    OnGameDLLQuery: Some(on_game_dll_query),
    OnMetamodQuery: Some(on_metamod_query),
    OnVSPListening: Some(on_vsp_listening),
    OnUnlinkConCommandBase: Some(on_unlink_con_command_base),
    OnUnlinkConCommand: Some(on_unlink_con_command),
    OnUnlinkConVar: Some(on_unlink_con_var),
};

/// The plugin object's address, as `CreateInterface("ISmmPlugin")` returns it.
#[must_use]
pub fn object() -> *mut c_void {
    core::ptr::addr_of!(PLUGIN.0).cast_mut().cast::<c_void>()
}

/// The plugin object's vtable, as the object stores it.
#[must_use]
pub fn vtable() -> *const VtEntry {
    core::ptr::addr_of!(VTABLE.0).cast::<VtEntry>()
}

/// The listener object's address, as `AddListener` receives it.
#[must_use]
pub fn listener() -> *mut IMetamodListener {
    core::ptr::addr_of!(LISTENER.0).cast_mut()
}

/// Whether Metamod is present: a `Load` completed and no `Unload` has since succeeded.
#[must_use]
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// The `ISmmAPI*` of the running Metamod, or null.
#[must_use]
pub fn ismm() -> *mut ISmmAPI {
    ISMM.load(Ordering::Acquire)
}

/// The `PluginId` Metamod gave this plugin.
#[must_use]
pub fn plugin_id() -> i32 {
    PLUGIN_ID.load(Ordering::Acquire)
}

/// Whether the listener has been handed to Metamod.
#[must_use]
pub fn listener_registered() -> bool {
    LISTENER_REGISTERED.load(Ordering::Acquire)
}

/// The table's values, one field per generated slot. This literal is what checks the
/// signatures: every function below has to have the argument and return types the generated
/// field declares (`*mut ISmmPlugin` first, `bool`/`i32`/`*const c_char` returns), so a wrong
/// shape is a compile error rather than a call into the wrong ABI.
///
/// The destructor slots come from `NULL` on both ABIs - their field names and shapes differ
/// per ABI (`dtor_complete` + `dtor_deleting` on Itanium, `dtor` on MSVC) - and
/// [`build_vtable`] fills them positionally; see there.
const TYPED_VTABLE: ISmmPlugin_vtable = ISmmPlugin_vtable {
    GetApiVersion: Some(get_api_version),
    Load: Some(load),
    AllPluginsLoaded: Some(all_plugins_loaded),
    QueryRunning: Some(query_running),
    Unload: Some(unload),
    Pause: Some(pause),
    Unpause: Some(unpause),
    GetAuthor: Some(get_author),
    GetName: Some(get_name),
    GetDescription: Some(get_description),
    GetURL: Some(get_url),
    GetLicense: Some(get_license),
    GetVersion: Some(get_version),
    GetDate: Some(get_date),
    GetLogTag: Some(get_log_tag),
    ..ISmmPlugin_vtable::NULL
};

/// Builds the `ISmmPlugin` vtable: every generated slot, plus the destructor slots between
/// `GetApiVersion` and `Load`.
///
/// The destructor is the one part the generated struct cannot hand over as a value here (its
/// shape differs per ABI: `dtor_complete` + `dtor_deleting` on Itanium, one `dtor` on MSVC), so
/// the slots are filled from the layout: every slot after `GetApiVersion` and before `Load` is a
/// destructor slot. That is one slot under MSVC and two under Itanium - the generated
/// `ISmmPlugin_slots::Load` (2 or 3) and `VTLEN` (16 or 17) say which, and the test at the
/// bottom checks the array against `size_of::<ISmmPlugin_vtable>()`.
///
/// One function serves both destructor shapes: Itanium's `~ISmmPlugin()` and `~ISmmPlugin()`-
/// deleting form take only `this`, MSVC's scalar-deleting `??_G` takes `(this, flags)` and
/// returns the freed pointer. A callee that declares `(this, flags) -> void*` is compatible with
/// a caller passing only `this` on x86_64 (the second register is simply not read, and a return
/// value the caller ignores costs nothing) - the same one-directional widening the tool object
/// uses for its `bool` slots. Metamod never destroys a plugin object in practice (they are the
/// plugin's own static singletons), so this is correctness for the table, not a live path.
#[must_use]
pub const fn build_vtable() -> [VtEntry; VT_LEN] {
    // SAFETY: `ISmmPlugin_vtable` is a `#[repr(C)]` struct whose every field is an
    // `Option<unsafe extern "C" fn ...>` - one pointer, no padding, no other data - and
    // `VtEntry` is `*const c_void`. The `const` block below pins both sizes, so a generator
    // change that made a slot something other than a function pointer would fail the build
    // instead of being reinterpreted here.
    let mut vt: [VtEntry; VT_LEN] = unsafe { core::mem::transmute(TYPED_VTABLE) };

    // The destructor slots: everything between GetApiVersion and Load, which is one slot under
    // MSVC and two under Itanium (the generated `ISmmPlugin_slots::Load` says which).
    let mut slot = ISmmPlugin_slots::GetApiVersion + 1;
    while slot < ISmmPlugin_slots::Load {
        vt[slot] = dtor as *const c_void;
        slot += 1;
    }

    vt
}

/// Writes `msg` into Metamod's error buffer as a NUL-terminated string of at most `maxlen`
/// bytes, truncated rather than overflowing (`ISmmPlugin.h:107`'s `error, maxlength`).
///
/// A null buffer or a zero length is a caller that does not want the message (Metamod passes
/// both to `QueryRunning`, `metamod_console.cpp:323`).
fn set_error(error: *mut c_char, maxlen: usize, msg: &str) {
    if error.is_null() || maxlen == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(maxlen - 1);
    // SAFETY: the caller guarantees `error` is writable for `maxlen` bytes (the ISmmPlugin
    // contract for every `char *error, size_t maxlen` pair), and `n <= maxlen - 1`.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), error, n);
        *error.add(n) = 0;
    }
}

/// `ISmmPlugin::GetApiVersion` (`core/ISmmPlugin.h:76-79`): the generated
/// `METAMOD_PLAPI_VERSION` (18).
unsafe extern "C" fn get_api_version(this: *mut ISmmPlugin) -> c_int {
    crate::guard(METAMOD_PLAPI_VERSION as c_int, || {
        let _ = this;
        METAMOD_PLAPI_VERSION as c_int
    })
}

/// The destructor slots (see [`build_vtable`]): the plugin object is a static, so there is
/// nothing to free; it answers null.
unsafe extern "C" fn dtor(this: *mut ISmmPlugin, flags: u32) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, flags);
        log::line("~ISmmPlugin (static object; nothing freed)");
        ptr::null_mut()
    })
}

/// `ISmmPlugin::Load` (`core/ISmmPlugin.h:107`): the `PLUGIN_SAVEVARS` block plus the listener.
///
/// The order is the macro's (`core/ISmmPlugin.h:514-518`): save `ISmmAPI*`, take the `IKHook*`
/// from `GetDetourInterface(id)`, then the plugin's own id. After that this plugin's extra
/// steps: install the `IKHook` into `s2sdk::hook`, and register the listener whose
/// `OnMetamodQuery` serves `McpBridge_001`.
///
/// Returns false with `error` filled if Metamod hands over no API (a `Load` with a null `ismm`
/// is a broken Metamod, not a state to run in), if `GetDetourInterface` answers null, or if the
/// `ISmmAPI` has no `GetDetourInterface`/`AddListener` slot; and it refuses a second `Load`
/// while this plugin is already loaded.
///
/// # Two Metamod plugins over one object
///
/// Metamod *can* hold two `CPlugin`s whose `m_API` is this one object - `dlmount` refcounts, so
/// loading the same file twice reaches the same static object - and `meta retry` is the path that
/// does it: its second `Load` arrives while the `CPlugin` whose load failed is still in the list,
/// and that `CPlugin`'s forced `_Unload` then calls `Unload` on the object the new copy is live
/// under. Because a duplicate `Load` would be for the *same* object, `is_present`'s refusal
/// cannot catch it: the load it would duplicate is the failed one, not the live one. See
/// [the retry section](crate::plugin#one-object-two-cplugins-meta-retry) for the sequence, what
/// is torn down, and why no plugin-side guard exists.
///
/// A `Load` for a *different* object of this crate (a second image under another path) is not a
/// duplicate: it has its own statics, and this refusal does not reach it.
///
unsafe extern "C" fn load(
    this: *mut ISmmPlugin,
    id: c_int,
    ismm: *mut ISmmAPI,
    error: *mut c_char,
    maxlength: usize,
    late: bool,
) -> bool {
    crate::guard(false, || {
        let _ = this;
        log::line(&format!(
            "Load(id={id}, late={late}, ismm={})",
            if ismm.is_null() { "null" } else { "set" }
        ));

        if ismm.is_null() {
            let msg = "cs2-mcp bridge: Load got a null ISmmAPI*; nothing to install";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        // never in a VAC-secured session (crate::vac): Metamod reports the refusal, nothing is
        // installed
        if !crate::vac::insecure_session() {
            set_error(error, maxlength, crate::vac::REFUSAL);
            log::line(crate::vac::REFUSAL);
            return false;
        }

        if is_present() {
            // Metamod never loads a running plugin twice; a second `Load` would register the
            // listener a second time (Metamod appends to the plugin's event list,
            // `core/metamod_plugins.cpp:103`'s `ITER_PLEVENT` walks every entry) and ask for a
            // second `IKHook`. Refusing keeps the one-listener invariant this plugin relies on.
            let msg = "cs2-mcp bridge: already loaded";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        // SAFETY: Metamod hands over a live `ISmmAPI` for the plugin's lifetime
        // (`core/ISmmPlugin.h:107`), and its vtable pointer is the object's own.
        let api = unsafe { ISmmAPI::vtable(ismm) };
        // Both slots are read before either is called: the `Option`s are how the generated
        // binding represents "this vtable has no such entry" (a real C++ vtable has none), so a
        // missing one is a broken object rather than a runtime state, and nothing should be
        // installed before both are known good.
        let (Some(get_detour), Some(add_listener)) = (api.GetDetourInterface, api.AddListener)
        else {
            let msg = "cs2-mcp bridge: ISmmAPI has no GetDetourInterface/AddListener slot";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        };
        // `PLUGIN_SAVEVARS` (`core/ISmmPlugin.h:516`).
        //
        // SAFETY: `id` is the plugin id Metamod just assigned to this plugin and `ismm` is its
        // live API object, which is the call's contract.
        let khook = unsafe { get_detour(ismm, id) };
        if khook.is_null() {
            let msg = "cs2-mcp bridge: ISmmAPI::GetDetourInterface returned null (no KHook)";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        ISMM.store(ismm, Ordering::Release);
        PLUGIN_ID.store(id, Ordering::Release);
        // This image now serves the Metamod role; the status line and every later log line say
        // so (`log::Role`). In practice the role was added already: `CreateInterface
        // ("ISmmPlugin")` is how this object was reached, and it adds the role before handing
        // the object out (`src/lib.rs`); this call is the same statement for a `Load` that
        // reached the object some other way. Roles are a set, so an image the tools framework
        // also serves ends up `mm+tool`.
        log::add_role(log::Role::Metamod);

        // SAFETY: the pointer is Metamod's live `IKHook` for this plugin, and it stays valid
        // until `Unload` (Metamod destroys it with the plugin).
        unsafe { khook_api::install(khook.cast()) };
        // From here on `s2sdk::hook`'s own diagnostics (a lost removal, a drain) go to the
        // bridge log next to the lines that explain them. A plain `fn(&str)`: the sink is called
        // while `s2sdk::hook`'s setup/removal gate is held, so it must not call the hook API -
        // `log::line` opens a file and writes one line, and touches nothing of ours.
        s2sdk::hook::set_diagnostic_sink(log::line);

        // `g_SMAPI->AddListener(this, this)` (`samples/s2_sample_mm/src/plugin.cpp:96`).
        //
        // SAFETY: both objects are this module's statics, live for the process, and Metamod
        // keeps the listener until this plugin unloads.
        unsafe { add_listener(ismm, object().cast::<ISmmPlugin>(), listener()) };
        LISTENER_REGISTERED.store(true, Ordering::Release);

        // This image is now a running Metamod plugin, so the wiring may start its listener:
        // `wiring::init` is the `IAppSystem::Init` gate of the C bridge (`mcp_bridge.c:6813`),
        // and for the Metamod role `Load` is the matching lifecycle start. The tools role calls
        // it from its own `Init` slot (`toolsys.rs`); both are the same statement.
        crate::wiring::init();

        // A previous unload may have asked the module verifier to stop; this is a fresh start,
        // so a verifier this image starts later must run again.
        crate::modules::live().resume_verify();

        // The tick on hosts without the tools framework (see `crate::gameframe`). A refusal is
        // not a load failure: the wire is served by the listener and the drain, and this is the
        // callback that drives them where no tools framework does. The status line reports the
        // reason (`gameframe::state`).
        //
        // SAFETY: `ismm` is the live `ISmmAPI` Metamod handed to this `Load`.
        match unsafe { crate::gameframe::install(ismm) } {
            Ok(()) => log::line("Load: tick installed (ISource2Server::GameFrame -> wiring::tick)"),
            Err(why) => log::line(&format!("Load: no GameFrame tick: {why}")),
        }

        // The plugin table (`metamod.plugins`): the manager is fetched once per `Load` and
        // the snapshot is built here, so a late `meta load` of this plugin starts from the
        // table as it stands (every other plugin is already in it). An unavailable manager is
        // logged and tolerated - `Load` still succeeds, and the row answers `blockedOn`
        // until a re-`Load` (`ops::metamod`'s docs). Both run on this (main) thread, where
        // the manager may be asked.
        // SAFETY: `ismm` is the live `ISmmAPI` Metamod handed `Load`, on the main thread
        // (`plugin::load`'s contract) - `fetch_plugin_manager`'s own safety contract.
        unsafe { crate::ops::metamod::fetch_plugin_manager(ismm) };
        crate::ops::metamod::rebuild();

        PRESENT.store(true, Ordering::Release);
        log::line("Load: Metamod present, KHook installed, listener registered");
        true
    })
}

/// `ISmmPlugin::AllPluginsLoaded` (`core/ISmmPlugin.h:116-118`): the plugin table's initial
/// rebuild is done, so the snapshot is refreshed once more.
///
/// Metamod fires `OnPluginLoad` for every plugin of the initial batch before this
/// (`core/metamod_plugins.cpp:249`), so the rows are already in the table; this rebuild is
/// the one that proves the batch settled (and lifts the scan bound over every id the batch
/// used). Runs on Metamod's (main) thread.
unsafe extern "C" fn all_plugins_loaded(this: *mut ISmmPlugin) {
    crate::guard((), || {
        let _ = this;
        crate::ops::metamod::rebuild();
        log::line("AllPluginsLoaded");
    });
}

/// `ISmmPlugin::QueryRunning` (`core/ISmmPlugin.h:141-144`): true while loaded, and false with
/// the reason in `error` otherwise.
///
/// The bridge has no "degraded" state Metamod should show as REFUSED: without KHook it refuses
/// the load, and once loaded it keeps running. Answering false is how a plugin tells Metamod
/// something is wrong (`meta list` shows REFUSED), so when it does answer false it says why:
/// no `Load` has completed in this image.
unsafe extern "C" fn query_running(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = this;
        if is_present() {
            return true;
        }
        let msg = "cs2-mcp bridge: no Metamod Load has completed in this image";
        set_error(error, maxlen, msg);
        false
    })
}

/// `ISmmPlugin::Unload` (`core/ISmmPlugin.h:154-157`): the runtime and hooks first, then the
/// state `Load` created, and false if KHook is not finished with this plugin's hooks.
///
/// Metamod calls this on *every* plugin it holds an `ISmmPlugin*` for - `_Unload` checks only
/// `m_API && m_Lib` (`core/metamod_plugins.cpp:653`) - including one whose `Load` returned
/// false, and `UnloadAll` calls it for all of them at shutdown and on `meta clear`. A plugin
/// that never loaded therefore answers true and touches nothing: with [`is_present`] false
/// there is no `IKHook` of ours, nothing to drain, and the tool object may be live.
///
/// The order for a loaded plugin:
///
/// 0. the runtime `Load` started, **only when this image does not serve the tool role**
///    ([`crate::wiring::stop_for_metamod_unload`]): the input/sound hooks off, the queue shut
///    down, the listener stopped (its threads joined, `session.json` deleted). Metamod
///    `dlclose`s this image once `Unload` answers true (`core/metamod_plugins.cpp:46-88`), so
///    those threads and their code cannot be left behind - and a later `meta load` in the same
///    process must be able to start a listener again (the one-shot is reset there). With the
///    tool role present the tools framework owns the listener and the queue, and its half keeps
///    running: the tool object's `Shutdown` stops it, not this call. When a listener thread
///    outlives the stop deadline the runtime keeps it (never detaches it) and this unload is
///    **refused** after the rest of the teardown, with
///    `UnloadTimeout: listener thread still running`: Metamod would otherwise unmap the image
///    under a running thread. The KHook API is kept on that refusal too (step 5 does not run):
///    a removal this image queued before the refusal - the GameFrame hook's `Drop`, or a drain
///    that timed out - may still run on KHook's delete thread, and a removal function entered
///    with no API would leave the registry unbarriered, so no retried `meta unload` could ever
///    drain it. A retried `meta unload` joins the thread once it exited and completes.
/// 1. [`crate::hooks::hooks_teardown`] - every hook owner's own uninstall, so its feature state
///    is cleared with its hook and nothing can re-arm one in between.
/// 2. [`crate::modules::Modules::wait_verifier`] - the verification thread must not be inside
///    the identity check while the API is forgotten: it holds a `&'static dyn KHookApi` from
///    `s2sdk::hook::api`. A verifier still running at the deadline refuses the unload
///    (`UnloadTimeout: verifier still running`). The thread the GameFrame tick started is
///    joined here too (bounded by the same deadline): it is not detached, so Metamod's
///    `dlclose` cannot happen under its exit path.
/// 3. [`s2sdk::hook::remove_all_blocking`] - every hook this module registered that an owner did
///    not release, removed synchronously and waited for. **Skipped while a `GameFrame` call is
///    on any thread** ([`crate::gameframe::in_frame`]): that stack holds the GameFrame hook's
///    capsule shared and this drain would spin on it without a deadline. The rest of the
///    teardown still runs; Metamod's own `Unloader` re-issues every id asynchronously and holds
///    the `dlclose` until each removal reports back (`core/metamod_plugins.cpp:46-88`), which
///    is exactly the skip's safety argument. The check is made twice (entry and immediately
///    before the drain), but a frame that starts on a pool thread between the second check and
///    the capsule lock still makes the synchronous re-issue wait for **one frame** rather than
///    for the deadline: `s2sdk::hook` has no per-id liveness query to wait for the queued
///    GameFrame removal first (`crate::gameframe`'s "The window the skip does not close").
/// 4. the listener registration: Metamod clears a plugin's listener list when it unloads it
///    (`core/metamod_plugins.cpp:658`), so there is no call to make - only our own flag.
/// 5. the `IKHook` is forgotten (Metamod deletes its `KHookImpl` as `Unload` returns true,
///    `core/metamod_plugins.cpp:59`, so the `IKHook*` dangles after that), and Metamod is
///    "absent" again.
///
/// Nothing else runs. In particular the tool object's shutdown sequence - `toolsys`'s
/// `Shutdown` slot and the C bridge's `m0_Shutdown` -> `m0_PreShutdown` -> "Shutdown done"
/// (`mcp_bridge.c:6763-6795`) - is not part of the Metamod lifecycle: `Load` did not
/// start it, the tools framework owns it, and an image that serves both objects must not have
/// its tool half torn down because Metamod unloaded its copy. That is why step 0 is gated on the
/// role and not run unconditionally.
///
/// # An unload issued from inside the frame
///
/// Which operator commands run from inside the server's frame is **unverified**; what matters
/// here is that an unload *can* be issued from inside one: another plugin's `GameFrame` hook
/// calling into Metamod is the way that happens. The forced paths - `meta force_unload`,
/// `UnloadAll` and `meta clear` (`core/metamod_console.cpp:561-575`,
/// `core/metamod_plugins.cpp:759`) - are **forced**, not in-frame paths: Metamod ignores our
/// `false` there. In a frame the full teardown runs and only step 3 is skipped, as above.
///
/// # A refused unload leaves the plugin inert
///
/// The refusal is the same state for every reason, and the image stays mapped (Metamod keeps the
/// plugin when `Unload` answers false, except for the forced callers listed below). Step 0 has
/// already stopped the runtime, the hook owners have already released their hooks (the GameFrame
/// tick included), and the verifier's stopping flag ([`crate::modules::Modules::begin_stop`]) is
/// set before any wait and **not** cleared: an unload cannot tell an ordinary `meta unload` from a
/// forced one (Metamod ignores the `false` for `UnloadAll`, `meta clear`, `meta force_unload`
/// and a retry), and a forced unload gives nobody a second chance to stop the thread. The KHook
/// API is the one piece of `Load`'s state a refusal does **not** undo (except the
/// `KHookNotInstalled` path, where it was already gone): it stays installed so a removal KHook
/// still has queued can run with it, and only a successful unload forgets it.
///
/// So in an **mm-only** image a refused unload leaves the plugin inert: no runtime, no GameFrame
/// hook (hence no tick), the verifier stopped. Nothing in this image clears the flag again until
/// `meta unload` is retried and succeeds or the process restarts; the refusal logs exactly that.
/// In an **mm+tool** image the tools framework's `RunFrame` keeps ticking, and
/// [`crate::wiring::tick`] clears the flag: a tick proves the image is still mapped and still
/// owns its state, which is the condition under which the verifier may safely run again.
///
/// # Ordering, if `Load` ever starts something that can register a hook
///
/// Today `Load` stores the API and id, installs the KHook adapter and the GameFrame hook,
/// registers the listener and starts the wiring. A later starter has to be stopped *before*
/// step 1: `remove_all_blocking`'s contract is "drop every `Hook` first, stop registering, then
/// call this", and a registration that lands after its snapshot is never re-issued.
///
/// # A timed-out drain returns false, and Metamod may override that
///
/// On `UnloadTimeout` this writes the message and returns false after the owners and the runtime
/// are already down. The drain that timed out is the case [`s2sdk::hook::Error::UnloadTimeout`]
/// documents as KHook having lost track of a hook (`RemoveHook` re-issued synchronously always
/// returns only once the removal function has retired, so a count that stays up after the loop
/// is the lost-hook race): what is left is a leaked context, not a live trampoline, and
/// `remove_all_blocking` has already given the ids up - a retry cannot re-issue them, so this
/// never recovers by itself.
///
/// # The forced-unload case: Metamod ignores the false
///
/// Metamod can still unload us: `_Unload` does `Unload(...) || force`
/// (`core/metamod_plugins.cpp:656`), and the callers that must win regardless pass
/// `force = true` - `UnloadAll` (`core/metamod_plugins.cpp:759`), `CPluginManager::Retry`'s
/// success path, which unloads the copy it superseded (`:372`), and that function's failure path
/// (`:387`). The refusal above - a listener thread still running, the verifier, the drain - is
/// therefore advisory: it stops an ordinary `meta unload`, but not a shutdown, a `meta clear` or
/// a `meta retry`.
///
/// `Retry` is the one caller that calls `_Unload` on a plugin whose object is *live under another
/// `CPlugin` at the same time*; see [the retry section](crate::plugin#one-object-two-cplugins-meta-retry)
/// for what that means for this object and why no plugin-side guard exists for it.
///
/// If a forced unload unmaps this image while a hook context is still live, exactly these
/// pointers are left dangling, and none of them can be repaired from the plugin side:
///
/// * the `IKHook*` this plugin stored (and `s2sdk::hook`'s installed API over it): it lives in
///   Metamod's `KHookImpl`, which the forced unload destroys; only a re-`Load` replaces it;
/// * the listeners: Metamod clears the plugin's event list (`:658`), so it will not call
///   `IMetamodListener` again - that part is safe;
/// * any hook KHook still associates with the plugin: its context, thunks and removal function
///   are code and data in the unmapped image. KHook would call them on the next invocation of
///   the hooked function or on a late removal - a jump into unmapped memory.
///
/// That is harmless only under one condition: the image really is unmapped and nothing KHook
/// still holds is ever called again. Metamod promises the first; it cannot promise the second
/// for a hook KHook lost track of (the `UnloadTimeout` case). This is why the timeout is
/// reported loudly and why its log line names the number of contexts left: after a forced
/// unload the only safe recovery is a process restart, not a re-`Load` in the same image.
///
/// Keeping the refusal (rather than logging and carrying on) is what this plugin's brief asks
/// for; a WP owner who prefers the leak logged and the unload completed should change this to
/// tear down after the message.
unsafe extern "C" fn unload(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = this;
        log::line("Unload");

        if !is_present() {
            // Metamod unloads plugins whose `Load` failed too (`core/metamod_plugins.cpp:650`
            // checks only that the plugin has an API and a library). Answer true and touch
            // nothing: there is no hook of ours to drain, and the tool object may be live.
            log::line("Unload: not loaded here; nothing to do");
            return true;
        }

        // The runtime `Load` started (step 0). Only when this image does not serve the tool
        // role: with the tools framework present it owns the listener and the queue, and the
        // tool object's own `Shutdown` stops them - a Metamod unload must leave that half
        // running. Without the role nothing else would ever stop them, and Metamod dlcloses this
        // image as soon as this answer is true (`core/metamod_plugins.cpp:46-88`). The answer is
        // part of the body's decision: a listener thread that outlives the stop deadline keeps
        // this image's code on a running thread, so the unload must refuse.
        let mut stop_runtime = || {
            if log::roles().contains(log::Role::Tool) {
                true
            } else {
                crate::wiring::stop_for_metamod_unload()
            }
        };

        // A verifier this image started (possible since the GameFrame tick, `crate::gameframe`)
        // must stop before its next call into KHook, whether or not the wait below succeeds: a
        // forced unload does not honour the refusal, and that is the one path where nobody else
        // will stop it. The flag stays set on a refusal: see the doc comment.
        crate::modules::live().begin_stop();

        // An unload from inside the frame skips only the synchronous drain: the frame's stack
        // holds the GameFrame hook's capsule shared, and `remove_all_blocking` would spin on it.
        // Metamod's `Unloader` holds the dlclose until every asynchronous removal reports back
        // (`core/metamod_plugins.cpp:46-88`), which is why the rest may still tear down here.
        let in_frame = crate::gameframe::in_frame();

        let loaded = unload_body(
            error,
            maxlen,
            in_frame,
            &mut stop_runtime,
            &mut |deadline| {
                let modules = crate::modules::live();
                if !modules.wait_verifier(deadline) {
                    return false;
                }
                // The wait proved the verifier's body finished (its flag is cleared at the end of
                // `verify_all`); the join then takes the thread out of this image, which Metamod
                // `dlclose`s once `Unload` answers true. A detached verifier would still be
                // executing its exit path in unmapped code.
                modules.join_verifier()
            },
            &mut || {
                // Re-checked here, not only at the entry: a frame can begin on another thread
                // (engine2's `"ServerGameFrame"` job) while the teardown above runs, and this
                // drain's synchronous removal would wait on that frame's capsule without a
                // deadline. The skip is the same answer as the entry's, for the same reason.
                if crate::gameframe::in_frame() {
                    log::line(
                        "Unload: a GameFrame call started during the teardown; the synchronous \
                         hook drain is skipped",
                    );
                    return Ok(());
                }
                s2sdk::hook::remove_all_blocking(unload_deadline())
            },
            &mut khook_api::uninstall,
        );
        if !loaded && !log::roles().contains(log::Role::Tool) {
            // One line for the operator of an mm-only image: there is no tool tick to clear
            // anything here. The runtime and the GameFrame hook are already down and the
            // verifier's stopping flag stays set, so the plugin is inert until `meta unload` is
            // retried - or the process restarts.
            log::line("Unload refused: plugin is inert until meta unload is retried");
        }
        loaded
    })
}

/// The loaded-plugin half of [`unload`], with the process-wide steps injectable: the runtime stop,
/// the module verifier wait, the KHook drain and the API forget. The extern above passes the live
/// ones; the tests drive this to pin the order and the refusal paths without a verifier thread, a
/// listener or a real `IKHook`.
///
/// The order is [`crate::hooks::unload_hooks`]'s contract: the runtime `Load` started, every hook
/// owner's own uninstall, the verifier wait, the process-wide drain as a backstop, then the API -
/// and the API only when the drain allows it **and** the runtime's listener threads are joined.
/// `in_frame` skips the drain (see [`unload`]): the owners' asynchronous drops are all that is
/// left for KHook's own `Unloader` to finish before the `dlclose`. The runtime answer decides the
/// final reply - and the API forget: a listener thread that is still running refuses the unload
/// and keeps the API installed, because the removals still queued in KHook may run before the
/// retry.
fn unload_body(
    error: *mut c_char,
    maxlen: usize,
    in_frame: bool,
    stop_runtime: &mut dyn FnMut() -> bool,
    wait_verifier: &mut dyn FnMut(Duration) -> bool,
    drain: &mut dyn FnMut() -> Result<(), s2sdk::hook::Error>,
    uninstall: &mut dyn FnMut(),
) -> bool {
    // Step 0: the runtime `Load` started (the extern decides whether this image may stop it).
    // Its answer is kept and decided on below, after the hooks are down: a listener thread that
    // outlives the deadline is the one thing that makes this image unsafe to unmap, and the
    // refusal must not leave this plugin anyway - the same inert state every other refusal leaves.
    let listener_joined = stop_runtime();
    // The API is forgotten only when the listener's threads are joined (see the closure's body).
    let mut uninstall_if_joined = || {
        if listener_joined {
            uninstall();
        } else {
            // The refusal below keeps this image mapped, and KHook may still run a removal
            // function of this module for a removal that is queued but not yet run (the
            // GameFrame hook's `Drop` on the in-frame path, or a drain that timed out). That
            // function needs the API: without it, `release_on_removal_by_id` reports
            // `unbarriered_removal`, and every later `remove_all_blocking` answers
            // `KHookNotInstalled` for ever (`s2sdk::hook`), so no retried `meta unload` could
            // ever complete. Keeping the API is also what the other refusals do (see the
            // `VerifierRunning` arm below), and Metamod keeps our `KHookImpl` alive while
            // `Unload` answers false (`core/metamod_plugins.cpp:656`).
            log::line(
                "hooks teardown: a listener thread is still running; the KHook API stays \
                 installed for the retry",
            );
        }
    };
    // The in-frame substitute for the synchronous drain: it asks KHook for nothing and says why.
    let mut skipped = || {
        log::line(
            "Unload: inside ISource2Server::GameFrame; the synchronous hook drain is skipped \
             (Metamod's Unloader holds the dlclose for the asynchronous removals)",
        );
        Ok(())
    };
    let drained = if in_frame {
        crate::hooks::unload_hooks(
            unload_deadline(),
            &mut *wait_verifier,
            &mut skipped,
            &mut uninstall_if_joined,
        )
    } else {
        crate::hooks::unload_hooks(
            unload_deadline(),
            &mut *wait_verifier,
            &mut *drain,
            &mut uninstall_if_joined,
        )
    };
    if let Err(err) = drained {
        // Only the two timeout shapes get the `UnloadTimeout:` prefix: that is the case a
        // Metamod operator or the bridge's own log greps for, and the one this plugin's brief
        // names. `KHookNotInstalled` (something left to remove and no API to remove it with - a
        // state Metamod never creates) says what it is.
        let msg = match &err {
            crate::hooks::UnloadError::VerifierRunning => {
                "UnloadTimeout: verifier still running".to_string()
            }
            crate::hooks::UnloadError::Hooks(s2sdk::hook::Error::UnloadTimeout { .. }) => {
                format!("UnloadTimeout: cs2-mcp bridge: {err}")
            }
            crate::hooks::UnloadError::Hooks(other) => {
                format!("cs2-mcp bridge: cannot remove the hooks: {other}")
            }
        };
        set_error(error, maxlen, &msg);
        log::line(&msg);
        // The refusal stands for every error, including `KHookNotInstalled` (today's
        // behaviour: `Unload` answers false and Metamod keeps the plugin, so a forced
        // unload is the only way past it). The one difference is inside `unload_hooks`,
        // which has already forgotten the API on that path: there is nothing left to drain
        // *with*, so keeping the adapter slot would only point the next `Load` at an object
        // Metamod no longer owns.
        //
        // For any other error the KHook API stays installed **on purpose**. A removal
        // function of this module that KHook runs later no longer reaches
        // `s2sdk::hook::api_expect()`: the removal path is admitted by id first, and a lost
        // (or already-fired) id is ignored without touching the API or the context. Keeping
        // the API means a tracked id can still be released, which also keeps this path
        // survivable if Metamod unmaps us anyway (its `_Unload` does `Unload(...) ||
        // force`). The listener flag and the Metamod state are left alone too: Metamod
        // still holds this plugin.
        //
        // `VerifierRunning` is the same answer for the same reason: the verifier thread is
        // inside this image asking KHook for `FindOriginal`, so the API must outlive it and
        // the unload can only be safe once it stopped. An operator retries `meta unload`; a
        // forced unload happens anyway, and the verifier's question is read-only for KHook
        // (it takes a shared lock, `detour.cpp:1898`), which is the least-bad exposure. The
        // stopping flag stays set: in an mm+tool image the next tick (`crate::wiring::tick`)
        // clears it, because a tick proves the image is still mapped; in an mm-only image this
        // refusal has also dropped the GameFrame hook, so nothing ticks again until the retry
        // succeeds or the process restarts (see [`unload`], "A refused unload leaves the plugin
        // inert").
        return false;
    }

    // Every hook is down and the API is still installed, but a listener thread of this image is
    // still running: Metamod would `dlclose` the image under it on this true. Refuse, and leave
    // the state `Load` set in place (the inert state the other refusals leave): a retried
    // `meta unload` stops/queries the runtime again and, once the thread has exited, joins it
    // and completes here. The API is kept on purpose (see `uninstall_if_joined`): the queued
    // asynchronous removals of this image may still run before the retry, and a removal function
    // entered with no API marks the registry `unbarriered_removal`, which turns every later
    // `remove_all_blocking` into `KHookNotInstalled` for the life of the process.
    if !listener_joined {
        let msg = "UnloadTimeout: listener thread still running";
        set_error(error, maxlen, msg);
        log::line(msg);
        return false;
    }

    // The hooks are gone and the API is forgotten (`unload_hooks`, with the listener joined);
    // the rest is `Load`'s state - including the plugin manager and the snapshot it served
    // (`ops::metamod`): once Metamod's `Unload` returns, Metamod's state is no longer ours to
    // touch, and a stale table must never be served as current.
    crate::ops::metamod::forget();
    LISTENER_REGISTERED.store(false, Ordering::Release);
    ISMM.store(ptr::null_mut(), Ordering::Release);
    PLUGIN_ID.store(0, Ordering::Release);
    PRESENT.store(false, Ordering::Release);
    log::line("Unload: hooks removed, KHook forgotten, Metamod absent");
    true
}

/// `ISmmPlugin::Pause` (`core/ISmmPlugin.h:166-169`): accepted, logged.
///
/// Nothing this plugin owns is pausable yet: the tool object is driven by the tools framework,
/// not by Metamod, and no detour is registered by default. Accepting keeps `meta pause` from
/// failing for no reason; when a detour exists it belongs here (remove it on pause, restore it
/// on unpause).
unsafe extern "C" fn pause(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = (this, error, maxlen);
        log::line("Pause (nothing to pause)");
        true
    })
}

/// `ISmmPlugin::Unpause` (`core/ISmmPlugin.h:178-181`): accepted, logged. See [`pause`].
unsafe extern "C" fn unpause(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = (this, error, maxlen);
        log::line("Unpause (nothing to unpause)");
        true
    })
}

/// `ISmmPlugin::GetAuthor` (`core/ISmmPlugin.h:184`).
unsafe extern "C" fn get_author(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"cs2-mcp".as_ptr()
    })
}

/// `ISmmPlugin::GetName` (`core/ISmmPlugin.h:187`).
unsafe extern "C" fn get_name(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"cs2-mcp bridge".as_ptr()
    })
}

/// `ISmmPlugin::GetDescription` (`core/ISmmPlugin.h:190`).
unsafe extern "C" fn get_description(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"CS2 MCP bridge: tool plugin and in-process MCP transport".as_ptr()
    })
}

/// `ISmmPlugin::GetURL` (`core/ISmmPlugin.h:193`): a string, but this plugin has no site.
///
/// The header's whole interface has no null option here (a `const char *` Metamod prints), and
/// there is no repository URL this plugin should claim, so the answer is the empty string -
/// what a C++ plugin without a homepage returns.
unsafe extern "C" fn get_url(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"".as_ptr()
    })
}

/// `ISmmPlugin::GetLicense` (`core/ISmmPlugin.h:196`): the workspace's MIT.
unsafe extern "C" fn get_license(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"MIT".as_ptr()
    })
}

/// `ISmmPlugin::GetVersion` (`core/ISmmPlugin.h:199`): the crate's version.
unsafe extern "C" fn get_version(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        VERSION_C.as_ptr()
    })
}

/// `ISmmPlugin::GetDate` (`core/ISmmPlugin.h:202`): the date of the plugin API generation
/// (not a build timestamp - the DLL is built with `/Brepro` and must be reproducible).
unsafe extern "C" fn get_date(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"2026-10-19".as_ptr()
    })
}

/// `ISmmPlugin::GetLogTag` (`core/ISmmPlugin.h:205`): the tag Metamod prefixes this plugin's
/// log lines with.
unsafe extern "C" fn get_log_tag(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"cs2-mcp".as_ptr()
    })
}

/// `CARGO_PKG_VERSION` as a NUL-terminated C string.
const VERSION_NUL: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
static VERSION_C: &core::ffi::CStr =
    match core::ffi::CStr::from_bytes_with_nul(VERSION_NUL.as_bytes()) {
        Ok(s) => s,
        Err(_) => panic!("CARGO_PKG_VERSION contains a NUL byte"),
    };

// ---------------------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------------------

/// `IMetamodListener::OnPluginLoad` (`core/ISmmPlugin.h:219`): a plugin joined the table -
/// lift the scan bound over its id and rebuild the snapshot (`ops::metamod`'s docs). Fires
/// after the load (`core/metamod_plugins.cpp:249`), on Metamod's (main) thread.
unsafe extern "C" fn on_plugin_load(this: *mut IMetamodListener, id: c_int) {
    crate::guard((), || {
        let _ = this;
        crate::ops::metamod::note_event_id(id);
        crate::ops::metamod::rebuild();
        log::line(&format!("OnPluginLoad({id})"));
    });
}

/// `IMetamodListener::OnPluginUnload` (`core/ISmmPlugin.h:228`): a plugin left the table -
/// rebuild, with the old id kept in the scan bound (`ops::metamod`). Fires after the unload
/// (`core/metamod_plugins.cpp:345`), on Metamod's (main) thread.
unsafe extern "C" fn on_plugin_unload(this: *mut IMetamodListener, id: c_int) {
    crate::guard((), || {
        let _ = this;
        crate::ops::metamod::note_event_id(id);
        crate::ops::metamod::rebuild();
        log::line(&format!("OnPluginUnload({id})"));
    });
}

/// `IMetamodListener::OnPluginPause` (`core/ISmmPlugin.h:237`): Metamod fires it after
/// `_Pause` set `Pl_Paused`, with the plugin's id (`ITER_PLEVENT(OnPluginPause, pl->m_Id)`,
/// `core/metamod_plugins.cpp:303-305`) - lift the scan bound over the id and rebuild, so a
/// pause on another plugin flips that row's status on the event, not one plugin event later.
/// Our own pause never reaches this listener (Metamod's event iteration skips the acting
/// plugin, `core/metamod_plugins.cpp:95-104`) - see [`crate::ops::metamod`]'s module docs.
unsafe extern "C" fn on_plugin_pause(this: *mut IMetamodListener, id: c_int) {
    crate::guard((), || {
        let _ = this;
        crate::ops::metamod::note_event_id(id);
        crate::ops::metamod::rebuild();
        log::line(&format!("OnPluginPause({id})"));
    });
}

/// `IMetamodListener::OnPluginUnpause` (`core/ISmmPlugin.h:246`): see [`on_plugin_pause`]
/// - same shape (`core/metamod_plugins.cpp:323-325`), the same rebuild.
unsafe extern "C" fn on_plugin_unpause(this: *mut IMetamodListener, id: c_int) {
    crate::guard((), || {
        let _ = this;
        crate::ops::metamod::note_event_id(id);
        crate::ops::metamod::rebuild();
        log::line(&format!("OnPluginUnpause({id})"));
    });
}

/// `IMetamodListener::OnLevelInit` (`core/ISmmPlugin.h:264-271`).
unsafe extern "C" fn on_level_init(
    this: *mut IMetamodListener,
    map_name: *const c_char,
    map_entities: *const c_char,
    old_level: *const c_char,
    landmark_name: *const c_char,
    load_game: bool,
    background: bool,
) {
    crate::guard((), || {
        let _ = (
            this,
            map_name,
            map_entities,
            old_level,
            landmark_name,
            load_game,
            background,
        );
        log::line("OnLevelInit");
    });
}

/// `IMetamodListener::OnLevelShutdown` (`core/ISmmPlugin.h:277`).
unsafe extern "C" fn on_level_shutdown(this: *mut IMetamodListener) {
    crate::guard((), || {
        let _ = this;
        log::line("OnLevelShutdown");
    });
}

/// The four `*Query` callbacks' default body, from the header's own implementations
/// (`core/ISmmPlugin.h:293-301`, `:315-323`, `:337-345`, `:359-367`): `*ret =
/// META_IFACE_FAILED; return NULL;`.
fn refuse_query(ret: *mut c_int) -> *mut c_void {
    if !ret.is_null() {
        // SAFETY: the caller guarantees `ret` is null or writable (`ISmmPlugin.h:290`).
        unsafe { *ret = META_IFACE_FAILED };
    }
    ptr::null_mut()
}

/// `IMetamodListener::OnEngineQuery` (`core/ISmmPlugin.h:293`): not ours, refused.
unsafe extern "C" fn on_engine_query(
    this: *mut IMetamodListener,
    iface: *const c_char,
    ret: *mut c_int,
) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, iface);
        refuse_query(ret)
    })
}

/// `IMetamodListener::OnPhysicsQuery` (`core/ISmmPlugin.h:315`): not ours, refused.
unsafe extern "C" fn on_physics_query(
    this: *mut IMetamodListener,
    iface: *const c_char,
    ret: *mut c_int,
) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, iface);
        refuse_query(ret)
    })
}

/// `IMetamodListener::OnFileSystemQuery` (`core/ISmmPlugin.h:337`): not ours, refused.
unsafe extern "C" fn on_file_system_query(
    this: *mut IMetamodListener,
    iface: *const c_char,
    ret: *mut c_int,
) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, iface);
        refuse_query(ret)
    })
}

/// `IMetamodListener::OnGameDLLQuery` (`core/ISmmPlugin.h:359`): not ours, refused.
unsafe extern "C" fn on_game_dll_query(
    this: *mut IMetamodListener,
    iface: *const c_char,
    ret: *mut c_int,
) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, iface);
        refuse_query(ret)
    })
}

/// `IMetamodListener::OnMetamodQuery` (`core/ISmmPlugin.h:380-388`): our own interface.
///
/// Metamod walks every listener for `MetaFactory` (`core/metamod.cpp:830-846`) and takes the
/// first non-null answer, so this writes `META_IFACE_OK` and returns the `McpBridge_001` object
/// when the name matches, and refuses everything else.
unsafe extern "C" fn on_metamod_query(
    this: *mut IMetamodListener,
    iface: *const c_char,
    ret: *mut c_int,
) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = this;
        // SAFETY: `iface` is Metamod's NUL-terminated interface name for the duration of the
        // call (`core/ISmmPlugin.h:380`).
        let matches = !iface.is_null()
            && unsafe { core::ffi::CStr::from_ptr(iface) } == crate::iface::IFACE_NAME;
        if !matches {
            return refuse_query(ret);
        }
        if !ret.is_null() {
            // SAFETY: the caller guarantees `ret` is null or writable.
            unsafe { *ret = META_IFACE_OK };
        }
        log::line("OnMetamodQuery(McpBridge_001) -> object");
        iface::object()
    })
}

/// `IMetamodListener::OnVSPListening` (`core/ISmmPlugin.h:413`): logged; this plugin never
/// calls `EnableVSPListener`, so it is only reached if another plugin turned VSP mode on.
unsafe extern "C" fn on_vsp_listening(this: *mut IMetamodListener, iface: *mut c_void) {
    crate::guard((), || {
        let _ = (this, iface);
        log::line("OnVSPListening");
    });
}

/// `IMetamodListener::OnUnlinkConCommandBase` (`core/ISmmPlugin.h:429`; deprecated since PLAPI
/// 17): logged.
unsafe extern "C" fn on_unlink_con_command_base(
    this: *mut IMetamodListener,
    id: c_int,
    command: *mut c_void,
) {
    crate::guard((), || {
        let _ = (this, id, command);
        log::line("OnUnlinkConCommandBase");
    });
}

/// `IMetamodListener::OnUnlinkConCommand` (`core/ISmmPlugin.h:441`): logged.
unsafe extern "C" fn on_unlink_con_command(
    this: *mut IMetamodListener,
    id: c_int,
    command: *mut c_void,
) {
    crate::guard((), || {
        let _ = (this, id, command);
        log::line("OnUnlinkConCommand");
    });
}

/// `IMetamodListener::OnUnlinkConVar` (`core/ISmmPlugin.h:453`): logged.
unsafe extern "C" fn on_unlink_con_var(
    this: *mut IMetamodListener,
    id: c_int,
    var: *mut c_void,
) {
    crate::guard((), || {
        let _ = (this, id, var);
        log::line("OnUnlinkConVar");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2sdk_sys::generated::IMetamodListener_slots;

    /// The array is the generated vtable, slot for slot: the same length in bytes, no hole, and
    /// each named slot at the generated index.
    #[test]
    fn vtable_matches_the_generated_layout() {
        let vt = build_vtable();
        assert_eq!(
            core::mem::size_of_val(&vt),
            core::mem::size_of::<ISmmPlugin_vtable>(),
            "the array must have the generated vtable's size"
        );
        assert_eq!(VT_LEN * core::mem::size_of::<usize>(), core::mem::size_of::<ISmmPlugin_vtable>());
        for (slot, entry) in vt.iter().enumerate() {
            assert!(!entry.is_null(), "ISmmPlugin slot {slot} is null");
        }
        assert_eq!(
            vt[ISmmPlugin_slots::GetApiVersion] as usize,
            get_api_version as *const () as usize
        );
        assert_eq!(vt[ISmmPlugin_slots::Load] as usize, load as *const () as usize);
        assert_eq!(vt[ISmmPlugin_slots::Unload] as usize, unload as *const () as usize);
        assert_eq!(
            vt[ISmmPlugin_slots::GetLogTag] as usize,
            get_log_tag as *const () as usize
        );
        // The generated length is the last named slot + 1 under MSVC; under Itanium the two
        // destructor slots sit in between, which is why the length is read from the generator
        // rather than recomputed.
        assert_eq!(ISmmPlugin_slots::VTLEN, s2sdk_sys::abi::vtable_len(
            s2sdk_sys::abi::Abi::native(),
            &s2sdk_sys::generated::ISmmPlugin_VCLASS
        ));
        // The destructor slots are the ones between GetApiVersion and Load.
        let dtor_slots =
            &vt[(ISmmPlugin_slots::GetApiVersion + 1)..ISmmPlugin_slots::Load];
        assert!(!dtor_slots.is_empty(), "every ABI has a destructor slot");
        for (offset, entry) in dtor_slots.iter().enumerate() {
            assert_eq!(
                *entry as usize,
                dtor as *const () as usize,
                "destructor slot {}",
                ISmmPlugin_slots::GetApiVersion + 1 + offset
            );
        }
    }

    /// The listener table has no null slot and every slot is where the generator says.
    #[test]
    fn listener_vtable_is_complete_and_at_the_generated_slots() {
        let vt = &LISTENER_VTABLE;
        assert_eq!(
            core::mem::size_of::<IMetamodListener_vtable>(),
            IMetamodListener_slots::VTLEN * core::mem::size_of::<usize>()
        );
        assert!(vt.OnPluginLoad.is_some());
        assert!(vt.OnPluginUnload.is_some());
        assert!(vt.OnPluginPause.is_some());
        assert!(vt.OnPluginUnpause.is_some());
        assert!(vt.OnLevelInit.is_some());
        assert!(vt.OnLevelShutdown.is_some());
        assert!(vt.OnEngineQuery.is_some());
        assert!(vt.OnPhysicsQuery.is_some());
        assert!(vt.OnFileSystemQuery.is_some());
        assert!(vt.OnGameDLLQuery.is_some());
        assert!(vt.OnMetamodQuery.is_some());
        assert!(vt.OnVSPListening.is_some());
        assert!(vt.OnUnlinkConCommandBase.is_some());
        assert!(vt.OnUnlinkConCommand.is_some());
        assert!(vt.OnUnlinkConVar.is_some());
    }

    /// The object is one vtable pointer, and it points at `VTABLE`.
    #[test]
    fn object_is_a_vtable_pointer() {
        assert_eq!(core::mem::size_of::<ISmmPlugin>(), core::mem::size_of::<usize>());
        // SAFETY: `object()` is this module's static, whose vtable pointer is `VTABLE`.
        let obj: *const ISmmPlugin = object().cast();
        assert_eq!(unsafe { (*obj).vtable }, vtable().cast::<c_void>());
    }

    /// `GetApiVersion` is 18 (`core/ISmmPluginExt.h:69`), the generated constant.
    #[test]
    fn api_version_is_the_generated_18() {
        assert_eq!(METAMOD_PLAPI_VERSION, 18);
        // SAFETY: `get_api_version` ignores `this`.
        assert_eq!(unsafe { get_api_version(ptr::null_mut()) }, 18);
    }

    /// A too-small error buffer is truncated and terminated, never overflowed.
    #[test]
    fn error_string_is_truncated_and_terminated() {
        let mut buf = [0x7fu8; 6];
        set_error(buf.as_mut_ptr().cast::<c_char>(), buf.len(), "abcdefghij");
        assert_eq!(&buf[..5], b"abcde");
        assert_eq!(buf[5], 0);
        // Null and zero length are no-ops.
        set_error(ptr::null_mut(), 8, "x");
        let mut buf = [0x7fu8; 4];
        set_error(buf.as_mut_ptr().cast::<c_char>(), 0, "x");
        assert_eq!(buf, [0x7f; 4]);
    }

    /// The verifier is waited out before the drain and the API forget, and the wait's answer is
    /// what decides: released -> the unload proceeds; never released -> the unload is refused
    /// with the brief's message and nothing after the wait ran.
    ///
    /// The wait itself (a held verifier thread woken by its condvar) is
    /// `tests/modules.rs`'s `wait_verifier_waits_for_the_running_verifier`; here the seam is the
    /// step's answer, so the order and the refusal can be pinned without a thread.
    #[test]
    fn unload_waits_for_the_verifier_before_the_drain() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("unload-verifier-wait");

        // Released: runtime -> verifier -> drain -> api, and the unload succeeds.
        let steps = std::cell::RefCell::new(Vec::new());
        let mut runtime = || {
            steps.borrow_mut().push("runtime");
            true
        };
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            true
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let mut err = [0 as c_char; 256];
        assert!(unload_body(
            err.as_mut_ptr(),
            err.len(),
            false,
            &mut runtime,
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        ));
        assert_eq!(
            *steps.borrow(),
            vec!["runtime", "verifier", "drain", "api"]
        );
        // The success path also cleared the Load state, as the extern does.
        assert!(!PRESENT.load(Ordering::Acquire));

        // Never released: refused before the drain, the API stays, the message is the brief's.
        PRESENT.store(true, Ordering::Release);
        let steps = std::cell::RefCell::new(Vec::new());
        let mut runtime = || {
            steps.borrow_mut().push("runtime");
            true
        };
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            false
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let mut err = [0 as c_char; 256];
        assert!(
            !unload_body(
                err.as_mut_ptr(),
                err.len(),
                false,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "a verifier still running refuses the unload"
        );
        assert_eq!(
            *steps.borrow(),
            vec!["runtime", "verifier"],
            "nothing after the wait ran: the verifier may still hold the API"
        );
        // SAFETY: `set_error` wrote a NUL-terminated string into the buffer.
        let message = unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(message, "UnloadTimeout: verifier still running");
        assert_eq!(
            scratch.count("UnloadTimeout: verifier still running"),
            1,
            "the refusal is in the log once: {}",
            scratch.text()
        );
        PRESENT.store(false, Ordering::Release);
    }

    /// A listener thread that outlives the stop deadline refuses the unload **after** the hooks
    /// are down (the same inert state the other refusals leave), with the brief's message; the
    /// KHook API is **not** forgotten (the removals still queued in KHook have to run with it),
    /// and once the thread exits, the retried unload stops the runtime again, joins it, and
    /// completes.
    #[test]
    fn a_listener_thread_that_misses_the_deadline_refuses_and_a_retry_succeeds() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("unload-listener-held");

        PRESENT.store(true, Ordering::Release);
        let steps = std::cell::RefCell::new(Vec::new());
        // The first attempt: the runtime stop cannot join its listener thread yet.
        let mut runtime = || {
            steps.borrow_mut().push("runtime:not joined");
            false
        };
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            true
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let mut err = [0 as c_char; 256];
        assert!(
            !unload_body(
                err.as_mut_ptr(),
                err.len(),
                false,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "a listener thread still running refuses the unload"
        );
        assert_eq!(
            *steps.borrow(),
            vec!["runtime:not joined", "verifier", "drain"],
            "the drain still ran; the API is the step the refusal keeps"
        );
        // SAFETY: `set_error` wrote a NUL-terminated string into the buffer.
        let message = unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(message, "UnloadTimeout: listener thread still running");
        assert_eq!(
            scratch.count("UnloadTimeout: listener thread still running"),
            1,
            "the refusal is in the log once: {}",
            scratch.text()
        );
        // A refusal does not forget the Metamod state, so the retry goes through the same body.
        assert!(PRESENT.load(Ordering::Acquire));

        // The retry, with the thread gone: the runtime joins it and everything completes.
        let steps = std::cell::RefCell::new(Vec::new());
        let mut runtime = || {
            steps.borrow_mut().push("runtime:joined");
            true
        };
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            true
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let mut err = [0 as c_char; 256];
        assert!(
            unload_body(
                err.as_mut_ptr(),
                err.len(),
                false,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "the retried unload succeeds once the listener threads are joined"
        );
        assert_eq!(
            *steps.borrow(),
            vec!["runtime:joined", "verifier", "drain", "api"]
        );
        assert_eq!(
            unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }.to_bytes(),
            b"",
            "the retry reports no error"
        );
        assert!(!PRESENT.load(Ordering::Acquire));
    }

    /// An unload from inside `GameFrame` runs the **full** teardown and skips only the
    /// synchronous drain: the owners' uninstalls and the verifier wait/join still run, the API
    /// is still forgotten, and the drain closure is never called. Metamod's `Unloader` holds the
    /// `dlclose` for the asynchronous removals (`core/metamod_plugins.cpp:46-88`), which is the
    /// skip's safety argument.
    #[test]
    fn an_unload_inside_the_frame_skips_only_the_drain() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("unload-in-frame");

        PRESENT.store(true, Ordering::Release);
        let steps = std::cell::RefCell::new(Vec::new());
        let mut runtime = || {
            steps.borrow_mut().push("runtime");
            true
        };
        let mut quiesce = |_deadline| {
            steps.borrow_mut().push("verifier");
            true
        };
        let mut drain = || {
            steps.borrow_mut().push("drain");
            Ok(())
        };
        let mut uninstall = || steps.borrow_mut().push("api");
        let mut err = [0 as c_char; 256];
        assert!(
            unload_body(
                err.as_mut_ptr(),
                err.len(),
                true,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "an in-frame unload answers true: {}",
            scratch.text()
        );
        assert_eq!(
            *steps.borrow(),
            vec!["runtime", "verifier", "api"],
            "runtime, verifier then API; the drain is the one step skipped"
        );
        assert_eq!(
            scratch.count("the synchronous hook drain is skipped"),
            1,
            "the skip is explained once: {}",
            scratch.text()
        );
        assert_eq!(
            unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }.to_bytes(),
            b"",
            "no error is reported"
        );
        assert!(!PRESENT.load(Ordering::Acquire));
    }

    /// The listener refusal keeps the KHook API (`uninstall` does not run), so the removal KHook
    /// still has queued runs **with** it, and the retried unload - whose drain is the production
    /// [`s2sdk::hook::remove_all_blocking`], not an `Ok` mock - drains and succeeds.
    ///
    /// The queue is what makes the difference: an in-frame unload skips the synchronous drain,
    /// so the GameFrame owner's `Drop` is still pending when the API would be forgotten, and
    /// KHook's delete thread runs the removal function afterwards.
    #[test]
    fn a_listener_refusal_keeps_the_api_for_the_queued_removal_and_the_retry() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("unload-listener-keeps-api");
        crate::khook_mock::reset();
        crate::khook_mock::install();

        // The removal an in-frame teardown queues: `Drop` asks KHook for an asynchronous
        // removal, and KHook's delete thread has not run the removal function yet.
        let hook = crate::khook_mock::register_async();
        drop(hook);
        assert_eq!(
            crate::khook_mock::queued(),
            1,
            "the removal is KHook's and has not run"
        );
        assert_eq!(
            s2sdk::hook::live_context_count(),
            1,
            "KHook still holds the context"
        );

        PRESENT.store(true, Ordering::Release);
        // Pass 1: inside the frame, and the listener thread missed the stop deadline.
        let mut runtime = || false;
        let mut quiesce = |_deadline| true;
        let mut drain = || panic!("an in-frame unload never calls the drain");
        let mut uninstall: fn() = s2sdk::hook::uninstall;
        let mut err = [0 as c_char; 256];
        assert!(
            !unload_body(
                err.as_mut_ptr(),
                err.len(),
                true,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "a listener thread still running refuses the unload"
        );
        assert_eq!(
            unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }.to_bytes(),
            b"UnloadTimeout: listener thread still running"
        );
        assert!(
            s2sdk::hook::api().is_some(),
            "the refusal keeps the API for the removal KHook still has queued"
        );
        assert_eq!(
            scratch.count("a listener thread is still running; the KHook API stays installed"),
            1,
            "the reason the API stays is in the log: {}",
            scratch.text()
        );

        // KHook's delete thread runs the queued removal function now. With the API installed it
        // releases the context; without one the registry would stay unbarriered.
        assert_eq!(crate::khook_mock::step_removals(), 1);
        assert_eq!(
            s2sdk::hook::live_context_count(),
            0,
            "the removal function released the context"
        );

        // The retry: out of frame, the listener joined - and the drain is the production
        // `remove_all_blocking`, which has to answer `Ok` rather than `KHookNotInstalled`.
        let mut runtime = || true;
        let mut quiesce = |_deadline| true;
        let mut drain = || s2sdk::hook::remove_all_blocking(unload_deadline());
        let mut uninstall: fn() = s2sdk::hook::uninstall;
        let mut err = [0 as c_char; 256];
        assert!(
            unload_body(
                err.as_mut_ptr(),
                err.len(),
                false,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "the retry drains and completes: {}",
            scratch.text()
        );
        assert_eq!(
            unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }.to_bytes(),
            b"",
            "the retry reports no error"
        );
        assert!(
            s2sdk::hook::api().is_none(),
            "a successful unload forgets the API"
        );
        assert!(!PRESENT.load(Ordering::Acquire));
    }

    /// NEGATIVE: the unconditional `uninstall` this refusal used to run. The API is forgotten
    /// with the removal still queued, so the removal function runs with no API (the registry's
    /// `unbarriered_removal`), and the production `remove_all_blocking` refuses every later
    /// retry with `KHookNotInstalled` - the state the listener refusal must never leave behind.
    #[test]
    fn the_old_unconditional_uninstall_refuses_every_later_retry() {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log("unload-listener-uninstalled");
        crate::khook_mock::reset();
        crate::khook_mock::install();

        let hook = crate::khook_mock::register_async();
        drop(hook);
        assert_eq!(crate::khook_mock::queued(), 1);

        PRESENT.store(true, Ordering::Release);
        let mut runtime = || false;
        let mut quiesce = |_deadline| true;
        let mut drain = || panic!("an in-frame unload never calls the drain");
        let mut uninstall: fn() = s2sdk::hook::uninstall;
        let mut err = [0 as c_char; 256];
        assert!(!unload_body(
            err.as_mut_ptr(),
            err.len(),
            true,
            &mut runtime,
            &mut quiesce,
            &mut drain,
            &mut uninstall,
        ));

        // The old behaviour: forget the API on the refusal, unconditionally.
        s2sdk::hook::uninstall();
        assert_eq!(
            crate::khook_mock::step_removals(),
            1,
            "the removal function ran with no API"
        );

        // The retry: the listener is joined and the frame is gone, so only the drain can refuse
        // it - and with no API the production drain answers `KHookNotInstalled`.
        let mut runtime = || true;
        let mut quiesce = |_deadline| true;
        let mut drain = || s2sdk::hook::remove_all_blocking(unload_deadline());
        let mut uninstall: fn() = s2sdk::hook::uninstall;
        let mut err = [0 as c_char; 256];
        assert!(
            !unload_body(
                err.as_mut_ptr(),
                err.len(),
                false,
                &mut runtime,
                &mut quiesce,
                &mut drain,
                &mut uninstall,
            ),
            "the retry is refused for ever once a removal ran with no API"
        );
        assert_eq!(
            unsafe { core::ffi::CStr::from_ptr(err.as_ptr()) }.to_bytes(),
            b"cs2-mcp bridge: cannot remove the hooks: no KHook IKHook API is installed (call s2sdk::hook::install first)"
        );
        assert_eq!(
            scratch.count("cannot remove the hooks: no KHook IKHook API is installed"),
            1,
            "the refusal is logged once: {}",
            scratch.text()
        );

        // Leave the process-wide registry as the other cases expect it: a drain with an API
        // clears `unbarriered_removal`.
        PRESENT.store(false, Ordering::Release);
        crate::khook_mock::install();
        let _ = s2sdk::hook::remove_all_blocking(std::time::Duration::from_secs(10));
        s2sdk::hook::uninstall();
    }
}
