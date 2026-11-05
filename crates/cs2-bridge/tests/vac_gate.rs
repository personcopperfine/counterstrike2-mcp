//! The `-insecure` gate (`cs2_bridge::vac`, crates/cs2-native FIXME 1): a Metamod `Load` in a
//! process without `-insecure`/`-dedicated` refuses before it installs anything, and a tool
//! `Init` there leaves the tick inert. Its own test binary, because the other plugin tests lift
//! the gate for their whole process.

use core::ffi::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Once;

use cs2_bridge::toolsys::{build_vtable, SLOT_RUN_FRAME};
use s2sdk_sys::generated::{IAppSystem_slots, ISmmAPI, ISmmPlugin, ISmmPlugin_slots};

/// The refusal lines go to a scratch log, never the user's `$XDG_RUNTIME_DIR/cs2mcpbridge`.
fn scratch_log() -> PathBuf {
    static SET: Once = Once::new();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch/cs2-bridge-vac-gate.log");
    SET.call_once(|| {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_file(&path);
        std::env::set_var("CS2_BRIDGE_LOG", &path);
    });
    path
}

#[test]
fn load_without_insecure_refuses_and_installs_nothing() {
    scratch_log();
    let object: *mut ISmmPlugin = cs2_bridge::plugin::object().cast();
    // never dereferenced: the gate answers before the ISmmAPI is used
    let ismm = core::ptr::NonNull::<ISmmAPI>::dangling().as_ptr();
    let mut error = [0 as c_char; 256];
    // SAFETY: the slot is the generated `Load` with the generated signature, on this crate's
    // static plugin object (first word = its vtable).
    let ok = unsafe {
        let vtable = (object.cast::<*const *const c_void>()).read();
        let load: unsafe extern "C" fn(*mut ISmmPlugin, c_int, *mut ISmmAPI, *mut c_char, usize, bool) -> bool =
            core::mem::transmute(*vtable.add(ISmmPlugin_slots::Load));
        load(object, 7, ismm, error.as_mut_ptr(), error.len(), false)
    };
    let text = unsafe { core::ffi::CStr::from_ptr(error.as_ptr()) }.to_string_lossy().into_owned();
    assert!(!ok, "Load must refuse without -insecure");
    assert_eq!(text, cs2_bridge::vac::REFUSAL);
    assert!(!cs2_bridge::plugin::is_present());
    assert!(!cs2_bridge::vac::insecure_session());
}

/// The tool role's `Init` without `-insecure` answers 1 (the tools start normally) and leaves the
/// bridge inert: the following frames count and log their "tick N" lines but never reach
/// `wiring::tick` (no module verifier, no listener, no drain).
#[test]
fn tool_init_without_insecure_leaves_the_tick_inert() {
    let log = scratch_log();
    let object = cs2_bridge::toolsys::object();
    let vt = build_vtable();
    // `wiring::tick` stores `mono_ms()`, which is 0 in this process's first millisecond: move the
    // clock past it, so a 0 below means "never ran"
    while cs2_bridge::wiring::mono_ms() < 2 {
        std::hint::spin_loop();
    }
    // SAFETY: the generated `Init` slot (`*mut c_void -> c_int`) and `SLOT_RUN_FRAME`
    // (`*mut c_void -> ()`) on the object `CreateInterface` hands out, the calls the tools
    // framework makes.
    let ok = unsafe {
        let init: unsafe extern "C" fn(*mut c_void) -> c_int = core::mem::transmute(vt[IAppSystem_slots::Init]);
        let tick: unsafe extern "C" fn(*mut c_void) = core::mem::transmute(vt[SLOT_RUN_FRAME]);
        let ok = init(object);
        for _ in 0..3 {
            tick(object);
        }
        ok
    };
    assert_eq!(ok, 1, "a refused Init still lets the tools start");
    assert_eq!(cs2_bridge::toolsys::ticks(), 3, "the frames are counted");
    assert_eq!(cs2_bridge::wiring::last_tick_ms(), 0, "wiring::tick never ran");
    assert_eq!(cs2_bridge::wiring::port(), None, "no listener");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(text.contains(cs2_bridge::vac::REFUSAL), "the refusal is logged: {text}");
    assert!(text.contains("tick 2"), "the tick lines still prove the tool is alive: {text}");
}
