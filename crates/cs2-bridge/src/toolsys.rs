//! The workshop tools' "tool plugin" object: the thing `CreateInterface("ToolSystem2_001")`
//! hands the tools framework, and the vtable it ticks every frame.
//!
//! Ported from the C bridge's `M0Obj` / `build_vtable` / `m0_*` block
//! (`bridge/mcp_bridge.c:6765-6839`). The C file's own note there says its vtable is
//! "IAppSystem (generated S2_SLOT_IAppSystem_*, 11 slots) + IToolSystem2's own slots 11..16";
//! the upstream declaration for the latter has since reached the generated bindings
//! (`crates/s2sdk/s2sdk-sys/src/generated/toolframework2.rs`), but that module is `cfg(windows)`
//! only, so the six numbers used here are spelled relative to the generated *length*
//! (`VTLEN_APPSYSTEM + 0..5`) - the form the WP brief sanctions - and tied back to the generated
//! `IToolSystem2_slots` names by the `const` assertions in [`ties`].
//!
//! Nothing here is a hand-written slot, offset or size: the eleven IAppSystem slots come from
//! the generated `IAppSystem_slots`, their order and length from the generated
//! `IAppSystem_VCLASS` (asserted in the tests), and the object's layout is the C struct's
//! `repr(C)` shape.

use core::ffi::{c_char, c_int, c_void};
use core::ptr::{self, addr_of};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use s2sdk_sys::generated::IAppSystem_slots;
use s2sdk_sys::generated::{kBuildTypeRelease, APP_SYSTEM_TIER3};

use crate::log;

/// Length of the generated IAppSystem vtable under the MSVC ABI the tools use.
///
/// The generated `VTLEN` const is the generator's own number - the same source of truth the C
/// header's `S2_VTLEN_IAppSystem` define carries (clang's vftable length for MSVC, the ABI-rule
/// walk for Itanium) - not a recompute from the last slot. A test asks the generator's own length
/// rule (`s2sdk_sys::abi::vtable_len`) for the same number, and the IToolSystem2 class built on
/// top of it is pinned by `ties` below (`HandleToolRequest + 1 == VT_LEN`), which the win64 check
/// compiles.
pub const VTLEN_APPSYSTEM: usize = IAppSystem_slots::VTLEN;

/// IToolSystem2's own virtuals, on the same vtable after IAppSystem's.
pub const TOOLSYSTEM2_OWN_SLOTS: usize = 6;

/// Number of entries in [`build_vtable`]'s array: IAppSystem's slots plus IToolSystem2's own.
pub const VT_LEN: usize = VTLEN_APPSYSTEM + TOOLSYSTEM2_OWN_SLOTS;

/// `IToolSystem2::InitToolSystem` (C `m0_Slot11`, `mcp_bridge.c:6811`): the tools framework's
/// per-frame hand-off.
pub const SLOT_INIT_TOOL_SYSTEM: usize = VTLEN_APPSYSTEM;
/// `IToolSystem2::ActivateTool` (C `m0_Slot12`).
pub const SLOT_ACTIVATE_TOOL: usize = VTLEN_APPSYSTEM + 1;
/// `IToolSystem2::RunFrame` (C `m0_Slot13`): **the tick** the WP0.4 proof reads off the log.
pub const SLOT_RUN_FRAME: usize = VTLEN_APPSYSTEM + 2;
/// `IToolSystem2::RequestQuit` (C `m0_Slot14`).
pub const SLOT_REQUEST_QUIT: usize = VTLEN_APPSYSTEM + 3;
/// `IToolSystem2::unk101` (C `m0_Slot15`; the name is the generator's verbatim guess).
pub const SLOT_UNK101: usize = VTLEN_APPSYSTEM + 4;
/// `IToolSystem2::HandleToolRequest` (C `m0_Slot16`).
pub const SLOT_HANDLE_TOOL_REQUEST: usize = VTLEN_APPSYSTEM + 5;

// The tick slot the WP brief names: length + 2 (the generated `IToolSystem2_slots::RunFrame`).
const _: () = assert!(SLOT_RUN_FRAME == VTLEN_APPSYSTEM + 2);
// The last written slot must stay inside the array the vtable is built from.
const _: () = assert!(SLOT_HANDLE_TOOL_REQUEST < VT_LEN);
const _: () = assert!(TOOLSYSTEM2_OWN_SLOTS == 6);

/// Ties the six relative slot numbers above to the generated declaration they belong to.
///
/// `toolframework2` (hence `IToolSystem2_slots`) is generated under `cfg(windows)` only, so these
/// assertions are compiled for Windows - which the acceptance gate covers with
/// `cargo check --target x86_64-pc-windows-msvc -p cs2-bridge`. Nothing is skipped on Linux: the
/// six numbers are the generated IAppSystem length + 0..5, and that length plus the tick offset
/// are what the tests at the bottom of this file check on the host.
#[cfg(windows)]
mod ties {
    use super::*;
    use s2sdk_sys::generated::IToolSystem2_slots as own;

    const _: () = assert!(own::InitToolSystem == SLOT_INIT_TOOL_SYSTEM);
    const _: () = assert!(own::ActivateTool == SLOT_ACTIVATE_TOOL);
    const _: () = assert!(own::RunFrame == SLOT_RUN_FRAME);
    const _: () = assert!(own::RequestQuit == SLOT_REQUEST_QUIT);
    const _: () = assert!(own::unk101 == SLOT_UNK101);
    const _: () = assert!(own::HandleToolRequest == SLOT_HANDLE_TOOL_REQUEST);
    const _: () = assert!(own::HandleToolRequest + 1 == VT_LEN);
    const _: () = assert!(own::Connect == IAppSystem_slots::Connect);
    const _: () = assert!(own::GetBuildType == IAppSystem_slots::GetBuildType);
}

/// One vtable entry.
///
/// The entries are `void *`, like the C bridge's `static void *g_vt[...]` (`mcp_bridge.c:6770`):
/// the slots have different signatures (void/int returns, opaque parameters) and the game calls
/// them by index, so the array has to pick one type for all of them. [`build_vtable`] is the only
/// place that fills it, and each assignment names the signature of its slot.
pub type VtEntry = *const c_void;

/// The object the tools framework sees (C `M0Obj`, `mcp_bridge.c:6765`).
#[repr(C)]
pub struct ToolObj {
    /// The vtable, as the game reads it: [`vtable`].
    pub vt: *const VtEntry,
    /// Set by `Connect` (C `self->factory = factory`).
    ///
    /// Atomic rather than the C's plain `void *`: it is the object's only mutable field, and an
    /// atomic store is what makes the object shareable without a data race. `AtomicPtr` has a
    /// pointer's own size and alignment, so the C layout is unchanged - the test below asserts
    /// the offsets. Nothing but `Connect` and the tests reads it.
    pub factory: AtomicPtr<c_void>,
    /// C `tool_idx`; the C bridge leaves it zero.
    pub tool_idx: u32,
    /// C `reserved`; the C bridge leaves it zero, and it is what keeps the object 24 bytes.
    pub reserved: u32,
}

/// The vtable array, immutable after the compile-time `const` build.
struct Vtable([VtEntry; VT_LEN]);

// SAFETY: a `Vtable` is only ever built by the `const` evaluation of `build_vtable` and stored in
// `VTABLE`; nothing writes through it (the entries are read-only function addresses), so sharing
// it between threads is sound.
unsafe impl Sync for Vtable {}

/// The object's vtable: built at compile time, so `CreateInterface` needs no lazy init and the
/// array cannot be half-built (the C bridge fills `g_vt` on the first call, `mcp_bridge.c:6846`).
static VTABLE: Vtable = Vtable(build_vtable());

/// The address of the vtable, as the object stores it.
const OBJ_VT: *const VtEntry = addr_of!(VTABLE.0) as *const VtEntry;

/// The one object handed out; `CreateInterface` returns its address on every call, like C's
/// `&g_obj`.
struct ObjCell(ToolObj);

// SAFETY: every field is written before the object's address is first handed out, and only
// `factory` is ever written afterwards - through an atomic, so a concurrent reader and writer do
// not race. From then on the object is immutable: `tool_idx` and `reserved` are read-only, and
// `vt` points at the read-only `VTABLE` (`Vtable`'s own `Sync`). The C bridge gets the same
// shape from its plain `g_obj` by convention.
unsafe impl Sync for ObjCell {}

static OBJ: ObjCell = ObjCell(ToolObj {
    vt: OBJ_VT,
    factory: AtomicPtr::new(ptr::null_mut()),
    tool_idx: 0,
    reserved: 0,
});

/// The factory the tools framework passed to `Connect`. C keeps the same value twice
/// (`self->factory` and `g_connectFactory`, `mcp_bridge.c:6811`).
static CONNECT_FACTORY: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// C `g_passthrough`: the object `InitToolSystem` hands over.
static PASSTHROUGH: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Whether the one-off "slot11 -> 1" line has been written.
static INIT_TOOL_SYSTEM_LOGGED: AtomicBool = AtomicBool::new(false);

/// Frame counter behind the "tick N" lines.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Set when [`init`] refused to start (no `-insecure`, [`crate::vac`]): the tick then only counts.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// The address of the tool object, as `CreateInterface` returns it.
#[must_use]
pub fn object() -> *mut c_void {
    // Handing the object out is the tools entry point (CreateInterface's `ToolSystem2_001`
    // branch); it adds the `tool` role to this image's role set (the same image may already be
    // the Metamod plugin - see `log::Role`).
    crate::log::add_role(crate::log::Role::Tool);
    addr_of!(OBJ.0) as *mut c_void
}

/// The vtable the object points at ([`VTABLE`]'s first entry).
#[must_use]
pub fn vtable() -> *const VtEntry {
    OBJ_VT
}

/// The factory `Connect` was given, or null before it ran.
#[must_use]
pub fn connect_factory() -> *mut c_void {
    CONNECT_FACTORY.load(Ordering::Acquire)
}

/// The passthrough object `InitToolSystem` was given, or null before it ran.
#[must_use]
pub fn passthrough() -> *mut c_void {
    PASSTHROUGH.load(Ordering::Acquire)
}

/// Ticks counted by [`run_frame`], the counter the "tick N" lines report.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Acquire)
}

/// Builds the vtable: the generated IAppSystem slots in their generated order, then
/// IToolSystem2's six own slots at `VTLEN_APPSYSTEM + 0..5`.
///
/// A `const fn` so the array is a compile-time value: a missed entry would be a null in a table
/// the game calls into, and the tests below refuse one.
#[must_use]
pub const fn build_vtable() -> [VtEntry; VT_LEN] {
    let mut vt = [ptr::null::<c_void>(); VT_LEN];

    // Each slot's signature, named by shape; the game calls by index, so the cast below is what
    // fixes the ABI of that entry.
    type ThisVoid = unsafe extern "C" fn(*mut c_void);
    type ThisInt = unsafe extern "C" fn(*mut c_void) -> c_int;
    type ThisPtr = unsafe extern "C" fn(*mut c_void) -> *const c_void;
    type ConnectFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    type QueryInterfaceFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
    type ReconnectFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char);
    type InitToolSystemFn = unsafe extern "C" fn(*mut c_void, *mut c_void, u8) -> c_int;
    type ActivateToolFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type Unk101Fn = unsafe extern "C" fn(*mut c_void, u8, u8);
    type HandleToolRequestFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

    // IAppSystem, all eleven, at the generated slots.
    vt[IAppSystem_slots::Connect] = connect as ConnectFn as *const c_void;
    vt[IAppSystem_slots::Disconnect] = disconnect as ThisVoid as *const c_void;
    vt[IAppSystem_slots::QueryInterface] = query_interface as QueryInterfaceFn as *const c_void;
    vt[IAppSystem_slots::Init] = init as ThisInt as *const c_void;
    vt[IAppSystem_slots::Shutdown] = shutdown as ThisVoid as *const c_void;
    vt[IAppSystem_slots::PreShutdown] = pre_shutdown as ThisVoid as *const c_void;
    vt[IAppSystem_slots::GetDependencies] = get_dependencies as ThisPtr as *const c_void;
    vt[IAppSystem_slots::GetTier] = get_tier as ThisInt as *const c_void;
    vt[IAppSystem_slots::Reconnect] = reconnect as ReconnectFn as *const c_void;
    vt[IAppSystem_slots::IsSingleton] = is_singleton as ThisInt as *const c_void;
    vt[IAppSystem_slots::GetBuildType] = get_build_type as ThisInt as *const c_void;

    // IToolSystem2's own six (see `ties` for the generated names).
    vt[SLOT_INIT_TOOL_SYSTEM] = init_tool_system as InitToolSystemFn as *const c_void;
    vt[SLOT_ACTIVATE_TOOL] = activate_tool as ActivateToolFn as *const c_void;
    vt[SLOT_RUN_FRAME] = run_frame as ThisVoid as *const c_void;
    vt[SLOT_REQUEST_QUIT] = request_quit as ThisInt as *const c_void;
    vt[SLOT_UNK101] = unk101 as Unk101Fn as *const c_void;
    vt[SLOT_HANDLE_TOOL_REQUEST] = handle_tool_request as HandleToolRequestFn as *const c_void;

    vt
}

/// The object pointer the game passes as `this`, as a `*mut ToolObj`.
///
/// # Safety
///
/// `this` must be the pointer [`object`] returned or null; the game never passes anything else,
/// because these functions are only reachable through [`VTABLE`].
unsafe fn this_obj(this: *mut c_void) -> Option<*mut ToolObj> {
    if this.is_null() {
        return None;
    }
    // `this` is our object (the caller's guarantee above), and `ToolObj` is the `repr(C)` C
    // `M0Obj` layout that pointer was produced from, so the cast itself is just a retype.
    Some(this.cast::<ToolObj>())
}

/// IAppSystem slot 0. C `m0_Connect` (`mcp_bridge.c:6811`): stores the factory, logs "Connect",
/// returns 1.
///
/// The return is `int` where upstream declares `bool`: on x64 a `bool` result is read out of AL,
/// and `eax = 1` has AL = 1, so the C bridge's shape is safe in this direction - and only this
/// one (a `bool`-returning slot called by an `int` caller would leave EAX's upper bits
/// undefined, which is why every slot here keeps the C bridge's `int`). Every return is exactly
/// 0 or 1, which is all either reader needs.
unsafe extern "C" fn connect(this: *mut c_void, factory: *mut c_void) -> c_int {
    crate::guard(1, || {
        log::line("Connect");
        CONNECT_FACTORY.store(factory, Ordering::Release);
        // SAFETY: `this` is our object, per `this_obj`; a null `this` stores only the global copy.
        if let Some(obj) = unsafe { this_obj(this) } {
            unsafe { (*obj).factory.store(factory, Ordering::Release) };
        }
        1
    })
}

/// IAppSystem slot 1. C `m0_Disconnect`: logs "Disconnect", nothing else.
unsafe extern "C" fn disconnect(this: *mut c_void) {
    crate::guard((), || {
        log::line("Disconnect");
        let _ = this;
    });
}

/// IAppSystem slot 2. C `m0_QueryInterface`: null.
unsafe extern "C" fn query_interface(this: *mut c_void, name: *const c_char) -> *mut c_void {
    crate::guard(ptr::null_mut(), || {
        let _ = (this, name);
        ptr::null_mut()
    })
}

/// IAppSystem slot 3. C `m0_Init`: logs "Init", returns 1.
///
/// The WP2.D tick's listener start is gated on this slot having run ([`crate::wiring::init`]):
/// the C's `m0_Slot13` is only ever called after `Init`, and making that explicit keeps a tick
/// on an object whose `Init` the framework refused from binding a port.
///
/// Without `-insecure` ([`crate::vac`]) the wiring is never initialised, so no listener binds and
/// no request is served, and [`run_frame`] stops calling [`crate::wiring::tick`] (no module
/// verifier, no Hammer fingerprint, no drain: nothing of ours runs in that session but the tick
/// counter). The slot still answers 1, because a refused `Init` would fail the whole tools
/// startup for the user, while an inert tool is invisible.
unsafe extern "C" fn init(this: *mut c_void) -> c_int {
    crate::guard(1, || {
        log::line("Init");
        let _ = this;
        if !crate::vac::insecure_session() {
            REFUSED.store(true, Ordering::Release);
            log::line(crate::vac::REFUSAL);
            return 1;
        }
        REFUSED.store(false, Ordering::Release);
        crate::wiring::init();
        1
    })
}

/// IAppSystem slot 4. C `m0_Shutdown`: logs "Shutdown", runs the pre-shutdown body, logs
/// "Shutdown done" - the C order, where `m0_Shutdown` calls `m0_PreShutdown`, so "PreShutdown"
/// appears between the two lines.
unsafe extern "C" fn shutdown(this: *mut c_void) {
    crate::guard((), || {
        let _ = this;
        shutdown_body();
    });
}

/// The object's shutdown sequence, as C `m0_Shutdown` runs it (`mcp_bridge.c:6783-6792`):
/// "Shutdown", then the pre-shutdown body ("PreShutdown"), then "Shutdown done".
///
/// Only the tool object's own `Shutdown` slot runs this. Metamod's `Unload` does not: what
/// `Load` started is the KHook adapter and the listener, and the tool half of an image belongs
/// to the tools framework ([`crate::plugin::unload`]).
fn shutdown_body() {
    log::line("Shutdown");
    pre_shutdown_body(ptr::null_mut());
    log::line("Shutdown done");
}

/// IAppSystem slot 5. C `m0_PreShutdown`: logs "PreShutdown", then the bridge teardown.
unsafe extern "C" fn pre_shutdown(this: *mut c_void) {
    crate::guard((), || pre_shutdown_body(this));
}

/// The shared body of `PreShutdown` and `Shutdown`.
fn pre_shutdown_body(this: *mut c_void) {
    log::line("PreShutdown");
    let _ = this;
    crate::wiring::pre_shutdown();
}

/// IAppSystem slot 6. C `m0_GetDependencies`: null.
unsafe extern "C" fn get_dependencies(this: *mut c_void) -> *const c_void {
    crate::guard(ptr::null(), || {
        let _ = this;
        ptr::null()
    })
}

/// IAppSystem slot 7. C `m0_GetTier`: the generated `APP_SYSTEM_TIER3`.
unsafe extern "C" fn get_tier(this: *mut c_void) -> c_int {
    crate::guard(APP_SYSTEM_TIER3 as c_int, || {
        let _ = this;
        APP_SYSTEM_TIER3 as c_int
    })
}

/// IAppSystem slot 8. C `m0_Reconnect`: nothing.
unsafe extern "C" fn reconnect(this: *mut c_void, factory: *mut c_void, name: *const c_char) {
    crate::guard((), || {
        let _ = (this, factory, name);
    });
}

/// IAppSystem slot 9. C `m0_IsSingleton`: 0.
unsafe extern "C" fn is_singleton(this: *mut c_void) -> c_int {
    crate::guard(0, || {
        let _ = this;
        0
    })
}

/// IAppSystem slot 10. C `m0_GetBuildType`: the generated `kBuildTypeRelease`.
unsafe extern "C" fn get_build_type(this: *mut c_void) -> c_int {
    crate::guard(kBuildTypeRelease as c_int, || {
        let _ = this;
        kBuildTypeRelease as c_int
    })
}

/// IToolSystem2 slot 0 (`SLOT_INIT_TOOL_SYSTEM`). C `m0_Slot11`: remembers the passthrough
/// object, logs "slot11 -> 1" once, returns 1.
///
/// The upstream declaration is `bool InitToolSystem(void *factory, uint8 nToolIndex)`; the C
/// bridge's parameter comment calls the first argument the per-frame passthrough object, and its
/// log line is kept verbatim. "once" is the WP brief's wording: the C bridge writes the line on
/// every call, this writes only the first, because a per-frame line would drown the tick lines
/// the WP0.4 proof reads.
///
/// `tool_index` is `u8` (upstream's `uint8`), not the C bridge's `u32`: a caller that passes a
/// byte leaves the upper 24 bits of the register undefined, so reading them would read garbage.
/// The value is unused either way - the narrower type just stops a later reader from comparing
/// garbage.
unsafe extern "C" fn init_tool_system(
    this: *mut c_void,
    passthrough: *mut c_void,
    tool_index: u8,
) -> c_int {
    crate::guard(1, || {
        let _ = (this, tool_index);
        PASSTHROUGH.store(passthrough, Ordering::Release);
        if !INIT_TOOL_SYSTEM_LOGGED.swap(true, Ordering::AcqRel) {
            log::line("slot11 -> 1");
        }
        1
    })
}

/// IToolSystem2 slot 1 (`SLOT_ACTIVATE_TOOL`). C `m0_Slot12`: nothing.
unsafe extern "C" fn activate_tool(this: *mut c_void, request: *mut c_void) {
    crate::guard((), || {
        let _ = (this, request);
    });
}

/// IToolSystem2 slot 2 (`SLOT_RUN_FRAME`): the tick. Counts frames, logs "tick N" for the
/// powers of two only (N = 1, 2, 4, 8, ...), then runs [`crate::wiring::tick`] - the C's slot
/// 13 body (`mcp_bridge.c:6809-6826`): modules poll, the once-only listener start and the
/// main-thread drain. After a refused [`init`] only the counter and its lines run.
unsafe extern "C" fn run_frame(this: *mut c_void) {
    crate::guard((), || {
        let _ = this;
        let n = TICKS.fetch_add(1, Ordering::AcqRel) + 1;
        if n.is_power_of_two() {
            log::line(&format!("tick {n}"));
        }
        if REFUSED.load(Ordering::Acquire) {
            return;
        }
        crate::wiring::tick();
    });
}

/// IToolSystem2 slot 3 (`SLOT_REQUEST_QUIT`). C `m0_Slot14`: 0.
unsafe extern "C" fn request_quit(this: *mut c_void) -> c_int {
    crate::guard(0, || {
        let _ = this;
        0
    })
}

/// IToolSystem2 slot 4 (`SLOT_UNK101`). C `m0_Slot15`: nothing.
///
/// Upstream declares the two arguments `bool, bool`; the C bridge passes them through as `u32`,
/// which is the same two argument registers on x64 but reads three bytes a `bool` caller never
/// set. They are `u8` here - the width a caller actually defines - and not Rust `bool`, which
/// would be undefined behaviour if the engine passed anything but 0 or 1.
unsafe extern "C" fn unk101(this: *mut c_void, a: u8, b: u8) {
    crate::guard((), || {
        let _ = (this, a, b);
    });
}

/// IToolSystem2 slot 5 (`SLOT_HANDLE_TOOL_REQUEST`). C `m0_Slot16`: logs "slot16", returns 0.
///
/// Upstream declares the return `void`; returning 0 for a `void` slot is ignored by the caller
/// and keeps the C bridge's `int` shape.
unsafe extern "C" fn handle_tool_request(this: *mut c_void, request: *mut c_void) -> c_int {
    crate::guard(0, || {
        let _ = (this, request);
        log::line("slot16");
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use s2sdk_sys::abi::{vtable_len, Abi};

    /// The WP brief's vtable assertions: the array is the generated MSVC IAppSystem length + 6,
    /// and the tick sits at that length + 2.
    #[test]
    fn vtable_is_generated_length_plus_six_with_the_tick_at_plus_two() {
        let vt = build_vtable();
        let appsystem = vtable_len(Abi::Msvc, &s2sdk_sys::generated::IAppSystem_VCLASS);

        assert_eq!(
            VTLEN_APPSYSTEM, appsystem,
            "IAppSystem's length must be the generated one"
        );
        assert_eq!(vt.len(), appsystem + 6);
        assert_eq!(vt.len(), VT_LEN);
        assert_eq!(
            SLOT_RUN_FRAME,
            appsystem + 2,
            "the tick is IAppSystem length + 2"
        );
    }

    /// No hole: every slot the game can reach addresses a function.
    #[test]
    fn vtable_has_no_null_entry() {
        let vt = build_vtable();
        for (slot, entry) in vt.iter().enumerate() {
            assert!(!entry.is_null(), "vtable slot {slot} is null");
        }
    }

    /// The tick slot really holds the tick, and `GetBuildType` the generated release build type.
    #[test]
    fn slots_point_at_their_functions() {
        let vt = build_vtable();
        assert_eq!(vt[SLOT_RUN_FRAME] as usize, run_frame as *const () as usize);
        assert_eq!(
            vt[IAppSystem_slots::GetBuildType] as usize,
            get_build_type as *const () as usize
        );
        assert_eq!(
            vt[IAppSystem_slots::Connect] as usize,
            connect as *const () as usize
        );
    }

    /// The object's layout is the C struct's (`void **vt; void *factory; u32 tool_idx;
    /// u32 reserved;` - `mcp_bridge.c:6765`): 24 bytes on x64.
    #[test]
    fn object_layout_matches_the_c_struct() {
        assert_eq!(core::mem::size_of::<ToolObj>(), 24);
        assert_eq!(core::mem::offset_of!(ToolObj, vt), 0);
        assert_eq!(core::mem::offset_of!(ToolObj, factory), 8);
        assert_eq!(core::mem::offset_of!(ToolObj, tool_idx), 16);
        assert_eq!(core::mem::offset_of!(ToolObj, reserved), 20);
    }

    /// The object points at the vtable, whose first entry is `IAppSystem::Connect`.
    #[test]
    fn object_points_at_the_vtable() {
        // SAFETY: `object()` is the live object this crate hands out and `vt` points at
        // `VTABLE.0`. Read through the raw pointer rather than a `&ToolObj`: the sibling test
        // writes `factory`, and a shared reference would be aliased by that write.
        let obj: *const ToolObj = object().cast();
        assert_eq!(unsafe { (*obj).vt }, vtable());
        assert_eq!(unsafe { *(*obj).vt } as usize, connect as *const () as usize);
    }

    /// `Connect` stores the factory and answers 1; the tick counts, through the real vtable
    /// entries (the WP0.4 behaviour, exercised on the host).
    #[test]
    fn connect_and_tick_through_the_vtable_entries() {
        let vt = build_vtable();
        let this = object();

        // SAFETY: each entry is the function `build_vtable` stored there, called with the
        // argument list its cast names, on the object pointer `CreateInterface` hands out.
        let r = unsafe {
            let connect: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int =
                core::mem::transmute(vt[IAppSystem_slots::Connect]);
            connect(this, 0x1234 as *mut c_void)
        };
        assert_eq!(r, 1);
        assert_eq!(connect_factory() as usize, 0x1234);
        // SAFETY: as above, `this` is not null, so the store landed in our object.
        assert_eq!(
            unsafe { (*this.cast::<ToolObj>()).factory.load(Ordering::Acquire) } as usize,
            0x1234
        );

        let before = ticks();
        // SAFETY: as above - `SLOT_RUN_FRAME` holds `run_frame`, `*mut c_void -> ()`.
        unsafe {
            let tick: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(vt[SLOT_RUN_FRAME]);
            for _ in 0..3 {
                tick(this);
            }
        }
        assert_eq!(ticks(), before + 3);
    }
}
