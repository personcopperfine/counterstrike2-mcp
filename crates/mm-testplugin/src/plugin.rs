//! The fixture's Metamod plugin object: `CreateInterface("ISmmPlugin")`, the `Load`/`Unload`
//! lifecycle and the one virtual KHook on `ISource2ServerConfig::AllowSaveRestore`.
//!
//! The pattern is the bridge's plugin (`crates/cs2-bridge/src/plugin.rs`): a static
//! `ISmmPlugin` object built over the generated [`ISmmPlugin_vtable`], `Load` doing the
//! `PLUGIN_SAVEVARS` block (`core/ISmmPlugin.h:514-518`), `Unload` draining the hooks before
//! anything else. This file's comments stay to the fixture-specific parts; Metamod's
//! `Load`/`Unload`/`meta retry` semantics (including the "one object, two `CPlugin`s" case)
//! are documented there and apply to this object identically - Metamod finds a plugin by the
//! object alone, calls `Load` at most once per live copy, and calls `Unload` for every copy
//! it holds, including one whose `Load` returned false. The same answers hold here: a
//! `Load` while loaded refuses, an `Unload` of a never-loaded plugin answers true and
//! touches nothing.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use core::time::Duration;
use std::sync::Arc;

use s2sdk::hook::{Hook, Removal, Return, SetupMode};
use s2sdk::module::{self, Module};
use s2sdk_sys::generated::{
    ISmmAPI, ISmmPlugin, ISmmPlugin_slots, ISmmPlugin_vtable, ISource2ServerConfig_slots,
    METAMOD_PLAPI_VERSION, SOURCE2SERVERCONFIG_INTERFACE_VERSION,
};

use crate::log;

/// The `META_IFACE_OK` of `core/ISmmPlugin.h:54`.
pub const META_IFACE_OK: c_int = 0;
/// The `META_IFACE_FAILED` of `core/ISmmPlugin.h:55`.
pub const META_IFACE_FAILED: c_int = 1;

/// How long `Unload` waits for KHook to run the removal functions of this plugin's hooks
/// before refusing the unload - the same reasoning as the bridge's `DEFAULT_UNLOAD_DEADLINE`
/// (`crates/cs2-bridge/src/plugin.rs`): five seconds is far longer than a healthy teardown
/// and far shorter than a `meta unload` operator's patience.
const UNLOAD_DEADLINE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------------------
// The mode
// ---------------------------------------------------------------------------------------

/// What the fixture's `pre` callback answers (`CS2MCP_TESTPLUGIN_MODE`, read at `Load`).
///
/// See [the crate docs](crate#the-three-modes-cs2mcp_testplugin_mode-read-at-load): KHook
/// folds several plugins' answers by the higher action (`khook.hpp:744-752`), and two value
/// changers on one call keep the *first saved* value (`detour.cpp:381-383`) - which is why
/// the supersede modes are the interesting ones against the bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `pre` answers `Ignore` and counts calls: the hook changes nothing.
    Passive,
    /// `pre` answers `Supersede(0)`: a value changer that disagrees with the bridge's 1.
    Supersede0,
    /// `pre` answers `Supersede(1)`: a value changer that agrees with the bridge.
    Supersede1,
}

impl Mode {
    /// The env-var spelling of this mode (the value `Load(mode=<m>)` logs).
    #[must_use]
    const fn name(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::Supersede0 => "supersede0",
            Self::Supersede1 => "supersede1",
        }
    }
}

/// Parse a `CS2MCP_TESTPLUGIN_MODE` value. The `Err` is the refusal message `Load` logs and
/// writes into Metamod's error buffer.
fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "passive" => Ok(Mode::Passive),
        "supersede0" => Ok(Mode::Supersede0),
        "supersede1" => Ok(Mode::Supersede1),
        other => Err(format!(
            "cs2-mcp test plugin: unknown CS2MCP_TESTPLUGIN_MODE '{other}' \
             (want passive|supersede0|supersede1)"
        )),
    }
}

/// The mode `Load` uses: `CS2MCP_TESTPLUGIN_MODE`, or `passive` when unset (or empty - an
/// env var with no value is not a mode, and the fixture's default is meant to be the state a
/// plain `meta load` of this plugin gets).
fn mode_from_env() -> Result<Mode, String> {
    let value = std::env::var("CS2MCP_TESTPLUGIN_MODE").unwrap_or_default();
    if value.is_empty() {
        return Ok(Mode::Passive);
    }
    parse_mode(&value)
}

// ---------------------------------------------------------------------------------------
// The hook target: `ISource2ServerConfig::AllowSaveRestore` on the object's own vptr
// ---------------------------------------------------------------------------------------

/// Where the hook goes: the `ISource2ServerConfig` object (whose first field is its vptr -
/// the table the game calls through) and the slot, which is always the generated
/// [`ISource2ServerConfig_slots::AllowSaveRestore`] - no hand-written number, per the repo
/// rule.
pub struct HookTarget {
    /// The `ISource2ServerConfig` object the server module's factory returned.
    pub object: *mut c_void,
    /// The generated slot index of `AllowSaveRestore` in the object's table.
    pub slot: i32,
}

/// How `Load` finds the target. The default is the live server; the tests point this at a
/// fake object, because a host test process has no `libserver.so` mapped (and the fixture
/// must not *need* one to be testable).
static RESOLVER: std::sync::Mutex<fn() -> Option<HookTarget>> = std::sync::Mutex::new(resolve_live_server);

/// Point `Load`'s target lookup at `f`.
///
/// A fixture for the tests: a real plugin never calls this. It is a plain `fn` pointer (no
/// captures) so the lookup stays trivially `Send`/`Sync`.
pub fn set_hook_target_resolver(f: fn() -> Option<HookTarget>) {
    *RESOLVER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = f;
}

/// The resolver `Load` currently uses - the value [`set_hook_target_resolver`] last stored,
/// so a fixture that swapped it in can swap the previous one back.
#[must_use]
pub fn hook_target_resolver() -> fn() -> Option<HookTarget> {
    *RESOLVER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The target `Load` hooks, through the resolver.
fn resolve_hook_target() -> Option<HookTarget> {
    (hook_target_resolver())()
}

/// The live target: the server module's `Source2ServerConfig001` object.
///
/// The object's first field is its vptr (`ISource2ServerConfig`'s generated struct: one
/// `vtable` pointer), which `Load` reads as `*(obj as *const *mut *mut c_void)`. That table
/// is what the game calls through, and the bridge pins the same `GSV_GAMECONFIG_SERVER_VT`
/// table on Windows: KHook patches the class's shared entry in place, so every caller of the
/// game's `AllowSaveRestore` enters it.
fn resolve_live_server() -> Option<HookTarget> {
    // The game's own server image, never Metamod's loader stub of the same name (Metamod maps
    // it first; `find_loaded` would answer it): the one finder the bridge uses too.
    let module = Module::find_game(module::names::SERVER).ok().flatten()?;
    let obj = module.create_interface(SOURCE2SERVERCONFIG_INTERFACE_VERSION)?;
    Some(HookTarget {
        object: obj.as_ptr(),
        slot: ISource2ServerConfig_slots::AllowSaveRestore as i32,
    })
}

// ---------------------------------------------------------------------------------------
// The hook signature: `bool ISource2ServerConfig::AllowSaveRestore()`
// ---------------------------------------------------------------------------------------

s2sdk::khook_thunks! {
    /// `bool ISource2ServerConfig::AllowSaveRestore()`, at the generated
    /// `ISource2ServerConfig_slots::AllowSaveRestore` slot. The thunk's machine return is
    /// `u8` - the C++ `bool`'s one byte - never a Rust `bool` across the ABI; the callbacks'
    /// `Return<u8>` carries it.
    pub mod allow_save_restore: fn(this: *mut c_void) -> u8 {
        sret_msvc = no_sret,
        sret_itanium = no_sret,
    }
}

// ---------------------------------------------------------------------------------------
// The evidence a live test reads
// ---------------------------------------------------------------------------------------

/// How many `pre` callbacks have run (`pre#<n>` in the log line).
static PRE_COUNT: AtomicU32 = AtomicU32::new(0);
/// How many `post` callbacks have run (`post#<n>` in the log line).
static POST_COUNT: AtomicU32 = AtomicU32::new(0);
/// The final value of the last driven call, or -1 when no call's `post` has run yet (or no
/// value pointer existed). A C++ `bool` is one byte, read as `u8` - never a Rust `bool`
/// across the ABI.
static LAST_FINAL: AtomicI32 = AtomicI32::new(-1);

/// How many `pre` callbacks have run so far.
#[must_use]
pub fn calls_pre() -> u32 {
    PRE_COUNT.load(Ordering::Acquire)
}

/// How many `post` callbacks have run so far.
#[must_use]
pub fn calls_post() -> u32 {
    POST_COUNT.load(Ordering::Acquire)
}

/// The final value of the last driven call (`-1`: none yet).
#[must_use]
pub fn last_final() -> i32 {
    LAST_FINAL.load(Ordering::Acquire)
}

/// Zero the counters and the recorded final value.
///
/// A fixture for the tests (and a restart of a live test harness): the counters are
/// monotonic across loads on purpose - they answer "how many calls since this image's
/// `Load`", and `Unload` does not zero them, so a host test that runs several `Load`s in one
/// process resets between cases.
pub fn reset_evidence() {
    PRE_COUNT.store(0, Ordering::Release);
    POST_COUNT.store(0, Ordering::Release);
    LAST_FINAL.store(-1, Ordering::Release);
}

// ---------------------------------------------------------------------------------------
// The ISmmPlugin object
// ---------------------------------------------------------------------------------------

/// Whether a `Load` has completed and no `Unload` has since succeeded.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// The `ISmmAPI*` Metamod passed to `Load` (`PLUGIN_SAVEVARS`'s `g_SMAPI`). Borrowed, never
/// freed (`core/ISmmPlugin.h:107`).
static ISMM: AtomicPtr<ISmmAPI> = AtomicPtr::new(ptr::null_mut());

/// The hook `Load` registered, as the `Box<Hook>` Metamod's `Unload` takes back. `Hook` is
/// `!Send` by design ("keep it on the thread that installed it"), and Metamod calls `Load`
/// and `Unload` on the same game thread - which is the assumption this static states, the
/// same one the C++ templates make. The `Box` indirection is only there to make the
/// non-`Send` type storable at all; the pointer is never dereferenced except by `Unload`
/// taking its `Hook` back.
static HOOK: AtomicPtr<Hook> = AtomicPtr::new(ptr::null_mut());

/// One vtable entry: `void *`, like the C bridge's arrays - the slots have different
/// signatures and the caller calls them by index.
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
// moved a slot would fail this crate's build rather than the game. Same asserts as the
// bridge's plugin (`crates/cs2-bridge/src/plugin.rs`).
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
/// Same shape as the bridge's `build_vtable` (`crates/cs2-bridge/src/plugin.rs`), for the
/// same reasons: the destructor's shape differs per ABI (one slot under MSVC, two under
/// Itanium - the generated `ISmmPlugin_slots::Load` and `VTLEN` say which), the one function
/// below serves both shapes (the callee declaring `(this, flags) -> void*` is compatible with
/// a caller passing only `this` on x86_64), and Metamod never destroys a plugin object in
/// practice (they are the plugin's own static singletons), so this is correctness for the
/// table, not a live path.
#[must_use]
pub const fn build_vtable() -> [VtEntry; VT_LEN] {
    // SAFETY: `ISmmPlugin_vtable` is a `#[repr(C)]` struct whose every field is an
    // `Option<unsafe extern "C" fn ...>` - one pointer, no padding, no other data - and
    // `VtEntry` is `*const c_void`. The `const` block in this module pins both sizes, so a
    // generator change that made a slot something other than a function pointer would fail
    // the build instead of being reinterpreted here.
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

/// Whether Metamod is present: a `Load` completed and no `Unload` has since succeeded.
#[must_use]
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Acquire)
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

/// `ISmmPlugin::Load` (`core/ISmmPlugin.h:107`): mode, `IKHook`, hook.
///
/// The `PLUGIN_SAVEVARS` order is the bridge's (`core/ISmmPlugin.h:514-518`): save
/// `ISmmAPI*`, take the `IKHook*` from `GetDetourInterface(id)`, then the plugin's own id.
/// After that, the fixture's steps: install the adapter into `s2sdk::hook` (exactly the
/// bridge's wiring - [`s2sdk::khook_vtable::install`] with this crate's log as the diagnostic
/// sink), then the one virtual KHook.
///
/// Returns false with `error` filled if the mode string is unknown, if Metamod hands over no
/// API, if `GetDetourInterface` answers null, if the `ISmmAPI` has no `GetDetourInterface`
/// slot, if no `ISource2ServerConfig` is reachable, if KHook refuses the hook, or if this
/// plugin is already loaded. Every refusal leaves nothing registered: either nothing was
/// installed yet, or the install is undone before returning.
///
/// # Safety
///
/// Metamod's own contract for the five arguments (`core/ISmmPlugin.h:107`): `ismm` the live
/// API, `error`/`maxlength` a writable buffer or null/0, and the call is on the game thread.
unsafe extern "C" fn load(
    this: *mut ISmmPlugin,
    id: c_int,
    ismm: *mut ISmmAPI,
    error: *mut c_char,
    maxlength: usize,
    late: bool,
) -> bool {
    crate::guard(false, || {
        let _ = (this, late);
        let mode = match mode_from_env() {
            Ok(mode) => mode,
            Err(msg) => {
                set_error(error, maxlength, &msg);
                log::line(&msg);
                return false;
            }
        };
        log::line(&format!("Load(mode={})", mode.name()));

        if ismm.is_null() {
            let msg = "cs2-mcp test plugin: Load got a null ISmmAPI*; nothing to install";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        if is_present() {
            // Metamod never loads a running plugin twice; a second `Load` would register the
            // hook a second time (and KHook would run our callbacks twice per call). Refusing
            // keeps the one-hook invariant, the same reason the bridge refuses.
            let msg = "cs2-mcp test plugin: already loaded";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        // SAFETY: Metamod hands over a live `ISmmAPI` for the plugin's lifetime
        // (`core/ISmmPlugin.h:107`), and its vtable pointer is the object's own.
        let api = unsafe { ISmmAPI::vtable(ismm) };
        let Some(get_detour) = api.GetDetourInterface else {
            let msg = "cs2-mcp test plugin: ISmmAPI has no GetDetourInterface slot";
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
            let msg = "cs2-mcp test plugin: ISmmAPI::GetDetourInterface returned null (no KHook)";
            set_error(error, maxlength, msg);
            log::line(msg);
            return false;
        }

        ISMM.store(ismm, Ordering::Release);

        // The adapter, exactly as the bridge's `Load` wires it
        // (`crates/cs2-bridge/src/plugin.rs`): both of s2sdk's diagnostic sinks go to this
        // crate's log, then the `IKHook` goes into `s2sdk::hook`.
        //
        // Two sinks, not one: `s2sdk::hook::set_diagnostic_sink` has no getter, so
        // `s2sdk::khook_vtable` cannot reuse its storage and keeps a second sink of its own
        // (see `khook_vtable.rs`'s `SINK`). Both are set; the lines are distinguished by
        // their `khook:` prefix.
        s2sdk::hook::set_diagnostic_sink(crate::log::line);
        s2sdk::khook_vtable::set_diagnostic_sink(crate::log::line);
        //
        // SAFETY: the pointer is Metamod's live `IKHook` for this plugin, and it stays valid
        // until `Unload` (Metamod destroys it with the plugin).
        unsafe { s2sdk::khook_vtable::install(khook.cast()) };

        let Some(target) = resolve_hook_target() else {
            let msg = "cs2-mcp test plugin: no ISource2ServerConfig in this process \
                       (server module not loaded)";
            set_error(error, maxlength, msg);
            log::line(msg);
            s2sdk::khook_vtable::uninstall();
            ISMM.store(ptr::null_mut(), Ordering::Release);
            return false;
        };
        // The vtable to hook is the one the object points at: `ISource2ServerConfig`'s
        // generated struct has one `vtable` pointer field, and that table is what the game
        // calls through (the bridge pins the same `GSV_GAMECONFIG_SERVER_VT` table on
        // Windows), so KHook's in-place patch reaches every caller.
        //
        // SAFETY: `target.object` is the live `ISource2ServerConfig` the resolver returned;
        // its first field is the vptr, stable for the process lifetime (a singleton the
        // engine owns).
        let vtable = unsafe { *(target.object as *const *mut *mut c_void) };

        let pre: allow_save_restore::PreCb = Arc::new(move |_call, _this| {
            // Every mode counts its calls; only the answer differs.
            PRE_COUNT.fetch_add(1, Ordering::AcqRel);
            match mode {
                Mode::Passive => Return::ignore(0),
                Mode::Supersede0 => Return::supersede_with(0u8),
                Mode::Supersede1 => Return::supersede_with(1u8),
            }
        });
        let post: allow_save_restore::PostCb = Arc::new(move |call, _this| {
            // The counter itself is an atomic add (defined to wrap); the display number
            // wraps with it rather than panicking on the wrap in a debug build.
            let post_n = POST_COUNT.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
            let final_value = match ptr::NonNull::new(call.current_value_ptr()) {
                // SAFETY: the pointer is KHook's per-call storage for this hook's `bool`
                // return (`khook.hpp:2193`), valid for the duration of the callback
                // (`s2sdk::hook::HookCall`'s contract); a C++ `bool` is one byte, read as
                // `u8` - never a Rust `bool` across the ABI.
                Some(value) => (unsafe { *value.as_ptr().cast::<u8>() }) as i32,
                None => -1,
            };
            LAST_FINAL.store(final_value, Ordering::Release);
            log::line(&format!(
                "AllowSaveRestore pre#{} post#{} final={final_value}",
                PRE_COUNT.load(Ordering::Acquire),
                post_n
            ));
            Return::ignore(0)
        });
        // KHook refuses a hook set up before its own `KHook` object is up with
        // `INVALID_HOOK` (`khook.hpp:2206-2231`); `setup_virtual` maps that to an error and
        // frees the context, so there is nothing left to undo except the adapter.
        //
        // SAFETY: `vtable` is the object's own vtable (read above from the resolver's live
        // object), the slot the generated index, and the thunks the generated ones for
        // `u8 (this)`.
        let hook = unsafe {
            allow_save_restore::setup_virtual(
                vtable,
                target.slot,
                vec![pre],
                vec![post],
                None,
                SetupMode::Sync,
            )
        };
        let hook = match hook {
            Ok(hook) => hook,
            Err(err) => {
                let msg = format!("cs2-mcp test plugin: hooking AllowSaveRestore failed: {err}");
                set_error(error, maxlength, &msg);
                log::line(&msg);
                s2sdk::khook_vtable::uninstall();
                ISMM.store(ptr::null_mut(), Ordering::Release);
                return false;
            }
        };
        let hook_id = hook.id();

        HOOK.store(Box::into_raw(Box::new(hook)), Ordering::Release);
        PRESENT.store(true, Ordering::Release);
        log::line(&format!("hooked AllowSaveRestore id={hook_id}"));
        true
    })
}

/// `ISmmPlugin::AllPluginsLoaded` (`core/ISmmPlugin.h:116-118`): logged, nothing else - the
/// fixture registers no hook that must wait for the mod.
unsafe extern "C" fn all_plugins_loaded(this: *mut ISmmPlugin) {
    crate::guard((), || {
        let _ = this;
        log::line("AllPluginsLoaded");
    });
}

/// `ISmmPlugin::QueryRunning` (`core/ISmmPlugin.h:141-144`): true while loaded, and false
/// with the reason in `error` otherwise (`meta list` shows REFUSED with the message).
unsafe extern "C" fn query_running(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = this;
        if is_present() {
            return true;
        }
        let msg = "cs2-mcp test plugin: no Metamod Load has completed in this image";
        set_error(error, maxlen, msg);
        false
    })
}

/// `ISmmPlugin::Unload` (`core/ISmmPlugin.h:154-157`): the hook first, then the drain, then
/// the adapter, in that order - and never from inside a callback.
///
/// A synchronous removal (`Hook::remove_sync`) from inside one of this hook's own callbacks
/// defers to the asynchronous path instead of spinning on the hooked address's mutex
/// (`detour.cpp:1503,1578-1601`), like any removal asked from inside a thunk - in every
/// build. Metamod calls `Unload` from its own code, not from a callback, and the fixture
/// registers no other callback path, so the synchronous path below holds by construction.
///
/// The drain after the removal is the bridge's `Unload` reasoning: `remove_sync` has already
/// asked KHook for the one hook this plugin owns, so the drain has nothing to re-issue in the
/// healthy case. It re-issues only ids KHook lost and never drained before
/// (`s2sdk::hook::remove_all_blocking`'s drain: the ids are permanent, so every call
/// re-issues all of them, but only the first per id is reported), it is skipped when there is
/// no API to drain with, and it is what waits out a [`Removal::Deferred`] (a `remove_sync`
/// that landed inside a thunk fell back to the asynchronous `Drop` path) - which is why it
/// runs even after a deferred removal. A timed-out drain refuses the unload
/// (`UnloadTimeout:`-prefixed, like the bridge's), which Metamod may override with a forced
/// unload - the same advisery refusal the bridge documents; see
/// `crates/cs2-bridge/src/plugin.rs`'s `Unload` for what a forced unload leaves dangling.
///
/// An `Unload` of a never-loaded plugin answers true and touches nothing: Metamod unloads
/// plugins whose `Load` failed too (`core/metamod_plugins.cpp:650`).
unsafe extern "C" fn unload(this: *mut ISmmPlugin, error: *mut c_char, maxlen: usize) -> bool {
    crate::guard(false, || {
        let _ = this;
        log::line("Unload");

        if !is_present() {
            log::line("Unload: not loaded here; nothing to do");
            return true;
        }

        // The hook `Load` boxed: take it back and remove it synchronously. `remove_sync`
        // consumes the `Hook` on every path (it is forgotten once the removal is asked or
        // found already done), so the slot is cleared first - a second `Unload` would find
        // nothing.
        let hook_ptr = HOOK.swap(ptr::null_mut(), Ordering::AcqRel);
        if !hook_ptr.is_null() {
            // SAFETY: `hook_ptr` is the `Box::into_raw` value `Load` stored, and the `Unload`
            // slot's contract (game thread, not inside a callback) is exactly what
            // `remove_sync` asks for.
            let hook = unsafe { Box::from_raw(hook_ptr) };
            match hook.remove_sync() {
                Ok(removal) => match removal {
                    Removal::Removed | Removal::AlreadyGone | Removal::ReconciledLost => {
                        log::line(&format!("Unload: removed ({removal:?})"));
                    }
                    // A `remove_sync` that landed inside one of this hook's own thunks
                    // (every build defers): the hook was handed to
                    // the asynchronous `Drop` path instead (`s2sdk::hook::Removal::
                    // Deferred`), the context is still live, and the drain below is what
                    // re-issues and waits for it - so the unload order is unchanged.
                    Removal::Deferred => {
                        log::line(
                            "Unload: removed (Deferred: asked from inside a thunk; \
                             the drain below waits for it)",
                        );
                    }
                },
                // `remove_sync` consumed the hook on the error paths too (it cannot be
                // removed without an API); the drain below would fail the same way, so this
                // refuses the unload and keeps the adapter installed, exactly like the
                // bridge's timeout path does.
                Err(err) => {
                    let msg = format!("cs2-mcp test plugin: cannot remove the hook: {err}");
                    set_error(error, maxlen, &msg);
                    log::line(&msg);
                    return false;
                }
            }
        }

        match s2sdk::hook::remove_all_blocking(UNLOAD_DEADLINE) {
            Ok(()) => {}
            Err(err) => {
                // The KHook API stays installed on this path on purpose: a removal function
                // of this module that KHook runs later reaches
                // `s2sdk::hook::api_expect()`, and a panic inside an `extern "C"` function
                // aborts the process. (The bridge's `Unload` documents this at length.)
                //
                // Only the timeout gets the `UnloadTimeout:` prefix - the case a Metamod
                // operator greps for, matching the bridge's own Unload
                // (`crates/cs2-bridge/src/plugin.rs`). An [`Error::InThunk`] gets its own
                // line: `Unload` ran on a hook thunk's stack, nothing was removed and
                // nothing was waited for - a refused call, not a timeout.
                let msg = match err {
                    s2sdk::hook::Error::UnloadTimeout { .. } => {
                        format!("UnloadTimeout: cs2-mcp test plugin: {err}")
                    }
                    s2sdk::hook::Error::InThunk => {
                        format!("cs2-mcp test plugin: unload refused: {err}")
                    }
                    other => format!("cs2-mcp test plugin: cannot remove the hooks: {other}"),
                };
                set_error(error, maxlen, &msg);
                log::line(&msg);
                return false;
            }
        }

        s2sdk::khook_vtable::uninstall();
        ISMM.store(ptr::null_mut(), Ordering::Release);
        PRESENT.store(false, Ordering::Release);
        log::line("Unload: hooks removed, KHook forgotten");
        true
    })
}

/// `ISmmPlugin::Pause` (`core/ISmmPlugin.h:166-169`): accepted, logged - the fixture's hook
/// belongs to no pause-relevant path.
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
        c"cs2-mcp test plugin".as_ptr()
    })
}

/// `ISmmPlugin::GetDescription` (`core/ISmmPlugin.h:190`).
unsafe extern "C" fn get_description(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"CS2 MCP coexistence fixture: one virtual KHook on AllowSaveRestore".as_ptr()
    })
}

/// `ISmmPlugin::GetURL` (`core/ISmmPlugin.h:193`): no site, the empty string (the header's
/// interface has no null option here).
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
/// (not a build timestamp - the image is built with `/Brepro` and must be reproducible).
unsafe extern "C" fn get_date(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"2026-10-20".as_ptr()
    })
}

/// `ISmmPlugin::GetLogTag` (`core/ISmmPlugin.h:205`): the tag Metamod prefixes this plugin's
/// log lines with.
unsafe extern "C" fn get_log_tag(this: *mut ISmmPlugin) -> *const c_char {
    crate::guard(ptr::null(), || {
        let _ = this;
        c"mm-testplugin".as_ptr()
    })
}

/// `CARGO_PKG_VERSION` as a NUL-terminated C string.
const VERSION_NUL: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
static VERSION_C: &core::ffi::CStr =
    match core::ffi::CStr::from_bytes_with_nul(VERSION_NUL.as_bytes()) {
        Ok(s) => s,
        Err(_) => panic!("CARGO_PKG_VERSION contains a NUL byte"),
    };

#[cfg(test)]
mod tests {
    use super::*;
    use s2sdk_sys::generated::ISource2ServerConfig_slots as cfg_slots;

    /// The array is the generated vtable, slot for slot (the bridge plugin's test, on this
    /// crate's own functions).
    #[test]
    fn vtable_matches_the_generated_layout() {
        let vt = build_vtable();
        assert_eq!(
            core::mem::size_of_val(&vt),
            core::mem::size_of::<ISmmPlugin_vtable>(),
            "the array must have the generated vtable's size"
        );
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
        assert_eq!(
            ISmmPlugin_slots::VTLEN,
            s2sdk_sys::abi::vtable_len(
                s2sdk_sys::abi::Abi::native(),
                &s2sdk_sys::generated::ISmmPlugin_VCLASS
            )
        );
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

    /// The object is one vtable pointer, and it points at `VTABLE`.
    #[test]
    fn object_is_a_vtable_pointer() {
        assert_eq!(core::mem::size_of::<ISmmPlugin>(), core::mem::size_of::<usize>());
        // SAFETY: `object()` is this module's static, whose vtable pointer is `VTABLE`.
        let obj: *const ISmmPlugin = object().cast();
        assert_eq!(unsafe { (*obj).vtable }, vtable().cast::<c_void>());
    }

    /// `GetApiVersion` is 18, the generated constant.
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

    /// Mode parsing: the three env spellings, and everything else refused with a message
    /// naming the three.
    #[test]
    fn mode_parsing_knows_exactly_the_three_values() {
        assert_eq!(parse_mode("passive"), Ok(Mode::Passive));
        assert_eq!(parse_mode("supersede0"), Ok(Mode::Supersede0));
        assert_eq!(parse_mode("supersede1"), Ok(Mode::Supersede1));
        let message = parse_mode("supersede").unwrap_err();
        assert!(message.contains("unknown CS2MCP_TESTPLUGIN_MODE 'supersede'"), "{message}");
        assert!(message.contains("passive|supersede0|supersede1"), "{message}");
        // An empty string is not a mode here (the env wrapper answers Passive for it).
        assert!(parse_mode("").is_err());
    }

    /// The generated slot is the one the resolver hands `Load`, and it stays 21 in the
    /// generated bindings this fixture hooks (a generator change moves it, and this test
    /// fails rather than the hook going to the wrong entry).
    #[test]
    fn the_hooked_slot_is_the_generated_allow_save_restore() {
        assert_eq!(cfg_slots::AllowSaveRestore, 21);
        assert_eq!(SOURCE2SERVERCONFIG_INTERFACE_VERSION, "Source2ServerConfig001");
        // The live resolver on a host answers nothing: no server module is mapped, and
        // `resolve_hook_target`'s contract is "no target, no hook".
        assert!(resolve_live_server().is_none());
    }
}
