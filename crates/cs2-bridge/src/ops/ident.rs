//! Object identity for the method bodies: the C's `mem_ok` / `vt_slots_ok` / `obj_is`
//! (`mcp_bridge.c:1182-1187`, `:1622-1641`), and `mod_iface` (`:4980-4987`).
//!
//! An object is "of the pinned class" when it is readable, its vptr is the pinned vtable
//! (`module base + vt_rva`), and every pinned slot of that vtable still holds its pinned target
//! (the same table [`crate::modules`] verified at load). This is the C's check with one
//! addition: a slot another KHook registration replaced is compared against the entry KHook
//! saved ([`crate::modules::slot_matches`]), because KHook patches the class's shared vtable in
//! place and never restores it (`third_party/khook/src/detour.hpp:96-104`, `SetupVirtual`
//! overwrites the entry and returns without restoring it). With no KHook
//! installed the check is exactly the C's.
//!
//! That addition has KHook's own limit, documented on [`crate::modules::slot_matches`]: KHook
//! keys `FindOriginalVirtual` on the slot address and answers the saved original whatever the
//! entry now holds, so a raw write on top of a slot KHook already hooked still passes. Only a
//! slot KHook never hooked fails closed.
//!
//! A relaxation met here is also recorded on the live module state
//! ([`crate::modules::Modules::record_relaxed`]), so a `control.capabilities` caller sees which
//! pins an object check had to relax even when the module was verified before KHook moved the
//! slot. The `relaxed:` line is written only when the pin is new for the module: object checks
//! run on every arm, and a repeated line per check would bury the log.

use core::ffi::c_void;

use crate::generated::pins::{self, PinSlot};
use crate::{log, modules};

/// `mem_ok` (`mcp_bridge.c:1182-1187`): whether one byte at `p` can be read without faulting.
#[must_use]
pub fn mem_ok(p: *const c_void) -> bool {
    if p.is_null() {
        return false;
    }
    let mut byte = 0u8;
    read_one(p, &mut byte)
}

#[cfg(windows)]
fn read_one(p: *const c_void, out: &mut u8) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut got = 0usize;
    // SAFETY: ReadProcessMemory on the current process validates the source range itself and
    // reports an unreadable one as failure instead of faulting; `out` is one writable byte and
    // `got` a valid usize, both live for the call.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            p,
            core::ptr::from_mut(out).cast(),
            1,
            &mut got,
        )
    };
    ok != 0 && got == 1
}

#[cfg(unix)]
fn read_one(p: *const c_void, out: &mut u8) -> bool {
    let local = libc::iovec {
        iov_base: core::ptr::from_mut(out).cast(),
        iov_len: 1,
    };
    let remote = libc::iovec {
        iov_base: p.cast_mut(),
        iov_len: 1,
    };
    // SAFETY: process_vm_readv on our own pid copies through the kernel, which checks the
    // remote range and returns -1/EFAULT for an unmapped one instead of faulting; `local`
    // describes one writable byte that outlives the call.
    let n = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    n == 1
}

/// `vt_slots_ok` (`mcp_bridge.c:1622-1634`) over the generated slot pins, KHook-aware.
#[must_use]
pub fn vt_slots_ok(module: u8, vt_rva: u32) -> bool {
    let mut relaxed = Vec::new();
    let ok = vt_slots_ok_relaxed(
        modules::live().base(module),
        module,
        vt_rva,
        pins::PIN_SLOTS,
        &mut relaxed,
    );
    // A slot KHook moved after verification is a fact about the module state, so the status
    // object reports it; the list dedupes and only a new pin writes its line.
    record_relaxed(module, relaxed);
    ok
}

/// [`vt_slots_ok`] against an explicit base and slot table (the host tests' fabricated image).
#[must_use]
pub fn vt_slots_ok_in(base: u64, module: u8, vt_rva: u32, slots: &[PinSlot]) -> bool {
    vt_slots_ok_relaxed(base, module, vt_rva, slots, &mut Vec::new())
}

/// Record the relaxations a check made on the live module state, writing each `relaxed:` line
/// only when [`crate::modules::Modules::record_relaxed`] inserted a new pin for `module`.
///
/// This is the one place an object check's relaxation reaches the log: the check itself
/// ([`crate::modules::slot_matches`]) only formats the line, because only the module state can
/// answer "have we said this already".
fn record_relaxed(module: u8, relaxed: Vec<modules::Relaxation>) {
    record_relaxed_on(modules::live(), module, relaxed);
}

/// [`record_relaxed`] against an explicit module state (the host test's own [`modules::Modules`],
/// which the process-wide `live()` cannot be on the host).
fn record_relaxed_on<P: modules::Platform + 'static>(
    state: &modules::Modules<P>,
    module: u8,
    relaxed: Vec<modules::Relaxation>,
) {
    for relaxation in relaxed {
        if state.record_relaxed(module, &relaxation) {
            log::line(&relaxation.line);
        }
    }
}

/// The one implementation: every pinned slot of the table this module carries at `vt_rva`.
fn vt_slots_ok_relaxed(
    base: u64,
    module: u8,
    vt_rva: u32,
    slots: &[PinSlot],
    relaxed: &mut Vec<modules::Relaxation>,
) -> bool {
    // The live table, for KHook's `FindOriginalVirtual`; the slot reads below go through the
    // same address.
    let vtable = base.wrapping_add(u64::from(vt_rva)) as *mut *mut c_void;
    for ps in slots
        .iter()
        .filter(|ps| ps.module == module && ps.vt_rva == vt_rva)
    {
        let entry = base + u64::from(vt_rva) + u64::from(ps.slot) * 8;
        // SAFETY: `base` is a verified module's load address and `vt_rva` + slot a pinned
        // vtable entry inside its image (checked byte-for-byte by the module gate before the
        // module is reported verified); the C reads the same word unguarded (`:1626`).
        let got = unsafe { (entry as *const u64).read_unaligned() };
        let want = base + u64::from(ps.target_rva);
        // SAFETY: `base` is a module's load address and `vt_rva + slot * 8` a pinned entry in
        // its vtable, so `vtable.offset(slot)` is readable (the `entry` read above just read
        // the same word).
        if !unsafe {
            modules::slot_matches(vtable, i32::from(ps.slot), got, want, ps.what, relaxed)
        } {
            log::line("iface: vtable slot changed since verification: ");
            log::line(ps.what);
            return false;
        }
    }
    true
}

/// `obj_is` (`mcp_bridge.c:1636-1641`): `obj` is a live object of the class whose vtable is
/// pinned at `vt_rva` in `module`.
#[must_use]
pub fn obj_is(obj: *const c_void, module: u8, vt_rva: u32) -> bool {
    let mut relaxed = Vec::new();
    let ok = obj_is_relaxed(
        obj,
        modules::live().base(module),
        module,
        vt_rva,
        pins::PIN_SLOTS,
        &mut relaxed,
    );
    record_relaxed(module, relaxed);
    ok
}

/// [`obj_is`] against an explicit base and slot table.
#[must_use]
pub fn obj_is_in(obj: *const c_void, base: u64, module: u8, vt_rva: u32, slots: &[PinSlot]) -> bool {
    obj_is_relaxed(obj, base, module, vt_rva, slots, &mut Vec::new())
}

/// [`obj_is_in`] with the relaxations the check made, for the tests (and for a future caller
/// that wants to report them): `relaxed` receives every pin [`crate::modules::slot_matches`]
/// accepted through KHook.
#[doc(hidden)]
pub fn obj_is_in_relaxed(
    obj: *const c_void,
    base: u64,
    module: u8,
    vt_rva: u32,
    slots: &[PinSlot],
    relaxed: &mut Vec<modules::Relaxation>,
) -> bool {
    obj_is_relaxed(obj, base, module, vt_rva, slots, relaxed)
}

/// The one implementation of [`obj_is`].
fn obj_is_relaxed(
    obj: *const c_void,
    base: u64,
    module: u8,
    vt_rva: u32,
    slots: &[PinSlot],
    relaxed: &mut Vec<modules::Relaxation>,
) -> bool {
    if obj.is_null() || !mem_ok(obj) {
        return false;
    }
    // SAFETY: `mem_ok` just proved the first byte readable; the C reads the vptr word the same
    // way (`:1638`). A vptr lies at offset 0 of every polymorphic object the callers pass.
    let vptr = unsafe { obj.cast::<u64>().read_unaligned() };
    // KHook never rewrites an object's vptr - it patches the shared table's entries - so the
    // vptr compare stays exact.
    if vptr != base + u64::from(vt_rva) {
        return false;
    }
    vt_slots_ok_relaxed(base, module, vt_rva, slots, relaxed)
}

/// `mod_iface` (`mcp_bridge.c:4980-4987`): `CreateInterface(version)` of the game's **own image**
/// of the module `module` (a pin index), null when that image is not mapped, when the loader's
/// mappings of the name are refused ([`s2sdk::module::Reject`]: only Metamod's loader stub, or
/// ambiguous), when the module exports no factory or when the factory answers null. Like the C,
/// the factory's return code is not consulted.
///
/// The image comes from the one game-module finder ([`s2sdk::module::Module::find_game`]), never
/// from a base-name query: `GetModuleHandleW("server.dll")` answers Metamod's loader stub, and
/// asking the stub for an interface forwards only while Metamod's core is up - it re-runs
/// Metamod's own load path (an extra `LoadLibrary` and a re-patched `Connect`) when Metamod's
/// gamedll bridge is null.
#[must_use]
pub fn mod_iface(module: u8, version: &str) -> *mut c_void {
    let Some(pin) = pins::PIN_MODULES.get(usize::from(module)) else {
        return core::ptr::null_mut();
    };
    match s2sdk::module::Module::find_game(pin.name) {
        Ok(Some(found)) => found
            .create_interface(version)
            .map_or(core::ptr::null_mut(), core::ptr::NonNull::as_ptr),
        Ok(None) => core::ptr::null_mut(),
        Err(reject) => {
            // One line per refused module per process: every row that needs the module would
            // otherwise repeat it.
            if !refusal_logged(module) {
                log::line(&format!("mod_iface: {}: {}", pin.name, reject.line()));
            }
            core::ptr::null_mut()
        }
    }
}

/// Whether this module's refusal was already logged (one line per module per process, like the
/// verifier's `verify:` refusal line).
fn refusal_logged(module: u8) -> bool {
    use core::sync::atomic::{AtomicU32, Ordering};
    /// One bit per pin index (`PIN_MODULE_COUNT` is 12).
    static REFUSED: AtomicU32 = AtomicU32::new(0);
    let bit = 1u32 << (module & 31);
    REFUSED.fetch_or(bit, Ordering::Relaxed) & bit != 0
}

#[cfg(test)]
mod tests {
    use super::{mem_ok, obj_is_in, obj_is_in_relaxed};
    use crate::generated::pins::PinSlot;
    use crate::hooks::{find_mock, test_guard};
    use crate::modules::{FileHash, Modules, Platform};
    use core::ffi::c_void;
    use std::sync::atomic::Ordering;

    /// A fabricated image: a "vtable" at RVA 0x10 whose slot 2 points at RVA 0x40.
    fn image() -> Vec<u64> {
        vec![0u64; 16]
    }

    #[test]
    fn obj_is_checks_vptr_and_every_pinned_slot() {
        let mut img = image();
        let base = img.as_ptr() as u64;
        img[2 + 2] = base + 0x40; // vtable at rva 0x10 = word 2; slot 2 = word 4
        let slots = [PinSlot {
            module: 3,
            vt_rva: 0x10,
            slot: 2,
            target_rva: 0x40,
            what: "test slot",
        }];
        let obj: [u64; 1] = [base + 0x10];
        let p = obj.as_ptr().cast::<c_void>();
        assert!(obj_is_in(p, base, 3, 0x10, &slots));
        // another vtable
        assert!(!obj_is_in(p, base, 3, 0x18, &slots));
        // the slot moved
        img[4] = base + 0x48;
        assert!(!obj_is_in(p, base, 3, 0x10, &slots));
        // a slot pin of another module does not apply
        assert!(obj_is_in(p, base, 4, 0x10, &slots));
        assert!(!obj_is_in(core::ptr::null(), base, 3, 0x10, &slots));
    }

    #[test]
    fn mem_ok_refuses_null_and_unmapped() {
        let x = 7u8;
        assert!(mem_ok(core::ptr::from_ref(&x).cast()));
        assert!(!mem_ok(core::ptr::null()));
        // the first page is never mapped on either target
        assert!(!mem_ok(0x10 as *const c_void));
    }

    /// A module state to record relaxations into: no module is ever loaded, which is all this
    /// test needs (`record_relaxed` only touches the string list and the caps revision).
    struct NoPlatform;

    impl Platform for NoPlatform {
        fn module_base(&self, _name: &str) -> Option<u64> {
            None
        }

        fn image(&self, _name: &str, _base: u64) -> Option<Vec<u8>> {
            None
        }

        fn file_hash(&self, _name: &str, _base: u64) -> FileHash {
            FileHash::NoName
        }

        fn pin(&self, _name: &str, _base: u64, _rva: u32) -> bool {
            false
        }
    }

    /// Review round 2: two identical object checks against a KHook'd slot record the pin once and
    /// write its `relaxed:` line once. The line belongs to the first observation of the pin, not
    /// to every arm that re-checks the object (object identity runs on each `game.saveRestore`,
    /// each sound install, ...).
    #[test]
    fn two_identical_checks_record_and_log_a_relaxation_once() {
        let _guard = test_guard::lock();
        let scratch = test_guard::scratch_log("ident-relaxed-once");
        find_mock::reset();

        // A fabricated table whose slot 3 holds KHook's thunk; KHook remembers the pin
        // (`base + 0x28`) as that slot's pre-hook original, keyed on the slot address
        // (`detour.cpp:1907-1915`).
        let mut table = [0u64; 8];
        let base = table.as_ptr() as u64;
        let want = base + 0x28;
        let thunk = 0x7f00_dead_beef_0000u64;
        table[3] = thunk;
        find_mock::HOOKED_SLOT.store(table.as_ptr() as usize + 3 * 8, Ordering::SeqCst);
        find_mock::SAVED.store(want as usize, Ordering::SeqCst);
        let mock: &'static find_mock::Mock = Box::leak(Box::new(find_mock::Mock));
        s2sdk::hook::install(mock);

        let state = Modules::new(NoPlatform);
        let revision = state.caps_revision();
        let pin = || PinSlot {
            module: 0,
            vt_rva: 0,
            slot: 3,
            target_rva: 0x28,
            what: "SYNTH_SLOT",
        };
        let obj = [base];

        let mut first = Vec::new();
        assert!(obj_is_in_relaxed(
            obj.as_ptr().cast(),
            base,
            0,
            0,
            &[pin()],
            &mut first
        ));
        assert_eq!(first.len(), 1, "the check relaxed the slot");
        super::record_relaxed_on(&state, 0, first);
        assert_eq!(scratch.count("relaxed: SYNTH_SLOT vt[3]=0x7f00deadbeef0000 original=0x"), 1);
        assert_eq!(state.relaxed(0), vec!["SYNTH_SLOT"]);
        assert_eq!(state.caps_revision(), revision + 1, "the new pin bumps caps");

        // The same check again: the object still passes, but nothing new is recorded or logged.
        let mut second = Vec::new();
        assert!(obj_is_in_relaxed(
            obj.as_ptr().cast(),
            base,
            0,
            0,
            &[pin()],
            &mut second
        ));
        assert_eq!(second.len(), 1, "the check still relaxes the slot");
        super::record_relaxed_on(&state, 0, second);
        assert_eq!(
            scratch.count("relaxed: SYNTH_SLOT"),
            1,
            "one line for the pin, not one per check: {}",
            scratch.text()
        );
        assert_eq!(state.relaxed(0), vec!["SYNTH_SLOT"], "listed once");
        assert_eq!(state.caps_revision(), revision + 1, "no second bump");
        assert!(
            std::fs::read_dir(scratch.xdg())
                .expect("the scratch XDG dir")
                .next()
                .is_none(),
            "the log line went to target/, not the user's XDG_RUNTIME_DIR"
        );

        s2sdk::hook::uninstall();
        find_mock::reset();
    }
}
