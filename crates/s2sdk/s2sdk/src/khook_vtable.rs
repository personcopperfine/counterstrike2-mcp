//! The `impl KHookApi` over Metamod's generated `IKHook_vtable`.
//!
//! `s2sdk::hook` talks to KHook through the [`KHookApi`] trait and nothing else; the object
//! behind that trait is Metamod's own `KHook::IKHook`, which `ISmmAPI::GetDetourInterface(id)`
//! returns (`core/metamod.cpp:1081`: `FindById(id)->m_khook`). This module is the adapter: one
//! method per virtual, in the generated vtable's order, every argument forwarded verbatim -
//! the `async` flag included, because KHook treats `true` and `false` differently on both
//! insert and removal (`detour.cpp:1716-1748`, `:1852-1867`).
//!
//! The vtable layout and every slot number come from the generated [`IKHook_vtable`] and
//! [`IKHook_slots`](s2sdk_sys::generated::IKHook_slots); nothing here indexes by a number.
//!
//! # Lifetime
//!
//! The `IKHook*` is borrowed from Metamod for the plugin's lifetime and never freed here
//! (`ISmmPlugin.h:107` gives the plugin a borrowed `ISmmAPI*`; KHook is the same kind of
//! object). [`install`] stores it, [`uninstall`] forgets it; the pointer is only handed back to
//! KHook itself, so it cannot outlive the plugin's use of it.
//!
//! # Diagnostics
//!
//! This crate has no log of its own, so the module's lines (a null slot, the
//! install/uninstall traces) go through [`set_diagnostic_sink`]: a `fn(&str)` the embedder
//! points at its own log (the bridge at `crate::log` from its plugin's `Load`,
//! `crates/cs2-bridge/src/khook_api.rs`). Unset - the default, and what this crate's own tests
//! run with - the lines are dropped, never guessed at.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::hook::{self, Action, HookId, KHookApi};
use s2sdk_sys::generated::{IKHook, IKHook_vtable};

/// Where [`missing_slot`] and [`install`]/[`uninstall`]'s trace lines go. Null until
/// [`set_diagnostic_sink`] is called; the default is no output.
///
/// The same shape [`crate::hook`]'s own diagnostic sink uses, for the same reason: a plain
/// `fn(&str)` is lock-free and callable from a removal function KHook runs while it holds its
/// own bookkeeping locks, where taking a lock of ours could deadlock with a caller that
/// already holds one.
static SINK: AtomicUsize = AtomicUsize::new(0);

/// Point this module's lines at `f`, typically the embedder's log function.
///
/// It can be called at any time from any thread (an embedder's `Load` is the intended place).
/// Like [`crate::hook::set_diagnostic_sink`], it is process-wide and not reset by
/// [`uninstall`]: whichever thread logs, the sink must outlive the embedder that set it,
/// exactly like the API slot.
pub fn set_diagnostic_sink(f: fn(&str)) {
    // A `fn` pointer is a thin pointer to code, so it round-trips through `usize` exactly.
    SINK.store(f as usize, Ordering::Release);
}

/// Hand `line` to the installed sink, if any.
fn emit(line: &str) {
    let stored = SINK.load(Ordering::Acquire);
    if stored == 0 {
        return;
    }
    // SAFETY: the only writer stores a `fn(&str)` cast to `usize` (see
    // `set_diagnostic_sink`), and the value is never cleared, so what is read here is either
    // 0 (rejected above) or that same function pointer. A plain `fn` pointer is a thin
    // pointer to code (no environment, unlike a closure `dyn Fn`), so the round trip is
    // lossless and this `transmute` is the exact inverse of the store-side cast.
    let f: fn(&str) = unsafe { core::mem::transmute(stored) };
    f(line);
}

/// The adapter: the `IKHook*` Metamod handed `Load`, or null when Metamod is absent or the
/// plugin has been unloaded.
pub struct VtableKhook {
    /// `IKHook*`. Set once per `Load`, cleared by `Unload`; every call reads it afresh so a
    /// re-load (Metamod can unload and reload a plugin) uses the new object.
    obj: AtomicPtr<IKHook>,
}

impl VtableKhook {
    /// A detached adapter; [`install`] points it at an object.
    #[must_use]
    const fn new() -> Self {
        Self {
            obj: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// The `IKHook*` this adapter currently forwards to, or null.
    #[must_use]
    fn target(&self) -> *mut IKHook {
        self.obj.load(Ordering::Acquire)
    }

    /// The object's vtable, or `None` when there is no object installed.
    ///
    /// # Safety
    ///
    /// When it answers `Some`, `this` is Metamod's live `IKHook` and the vtable is its own.
    unsafe fn vtable(&self) -> Option<&'static IKHook_vtable> {
        let this = self.target();
        if this.is_null() {
            return None;
        }
        // SAFETY: `install` only ever stores the pointer `ISmmAPI::GetDetourInterface`
        // returned, which Metamod keeps alive for the plugin's lifetime
        // (`core/metamod.cpp:1081`), and `IKHook::vtable` reborrows it.
        Some(unsafe { IKHook::vtable(this) })
    }
}

/// The adapter `Load` installs. One per process, like the `IKHook` object itself.
static KHOOK: VtableKhook = VtableKhook::new();

/// Point the adapter at `obj` and install it as `s2sdk::hook`'s API.
///
/// # Safety
///
/// `obj` must be the live `IKHook` `ISmmAPI::GetDetourInterface` returned for this plugin, and
/// must stay valid until [`uninstall`] runs.
pub unsafe fn install(obj: *mut IKHook) {
    KHOOK.obj.store(obj, Ordering::Release);
    hook::install(&KHOOK);
    emit("khook: IKHook installed into s2sdk::hook");
}

/// Forget the API. Only safe once no [`s2sdk::hook::Hook`] is alive any more - the embedder's
/// `Unload` runs [`s2sdk::hook::remove_all_blocking`] first for exactly that reason.
pub fn uninstall() {
    hook::uninstall();
    KHOOK.obj.store(ptr::null_mut(), Ordering::Release);
    emit("khook: IKHook uninstalled");
}

/// Report a slot the generated vtable left null.
///
/// A `null` slot in Metamod's `IKHook` would be a broken object (the real class implements
/// every virtual, `khook.hpp:2184-2201`), so this is loud in the log and the call answers the
/// neutral value its slot's type has. It cannot happen with a real Metamod.
fn missing_slot(slot: &str) {
    emit(&format!("khook: IKHook::{slot} is null"));
}

// The trait declares `find_original`/`find_original_virtual`/`lookup_signature` safe while they
// take raw pointers, because their contracts (a live vtable, a readable search range) belong to
// the callers inside a thunk - `s2sdk::hook` documents them per method and the trait cannot be
// changed from here. Forwarding them verbatim is exactly what this adapter is for, so the
// "might dereference a raw pointer" lint has nothing to add.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
impl KHookApi for VtableKhook {
    /// `IKHook::SetupHook` (`khook.hpp:2186`). Every argument, `async` included, is the one
    /// `s2sdk::hook` passed.
    unsafe fn setup_hook(
        &self,
        function: *mut c_void,
        context: *mut c_void,
        removed_function: *mut c_void,
        pre: *mut c_void,
        post: *mut c_void,
        make_return: *mut c_void,
        make_call_original: *mut c_void,
        stack_size: u32,
        r#async: bool,
    ) -> HookId {
        // SAFETY: the trait's caller guarantees the function and thunk addresses and the
        // context's lifetime; the vtable is Metamod's own.
        let vt = match unsafe { self.vtable() } {
            Some(vt) => vt,
            None => {
                missing_slot("SetupHook (no IKHook)");
                return hook::INVALID_HOOK;
            }
        };
        let Some(f) = vt.SetupHook else {
            missing_slot("SetupHook");
            return hook::INVALID_HOOK;
        };
        // SAFETY: as above; the pointer is Metamod's live object.
        unsafe {
            f(
                self.target(),
                function,
                context,
                removed_function,
                pre,
                post,
                make_return,
                make_call_original,
                stack_size,
                r#async,
            )
        }
    }

    /// `IKHook::SetupVirtualHook` (`khook.hpp:2187`).
    #[allow(clippy::too_many_arguments)]
    unsafe fn setup_virtual_hook(
        &self,
        vtable: *mut *mut c_void,
        index: i32,
        context: *mut c_void,
        removed_function: *mut c_void,
        pre: *mut c_void,
        post: *mut c_void,
        make_return: *mut c_void,
        make_call_original: *mut c_void,
        stack_size: u32,
        r#async: bool,
    ) -> HookId {
        // SAFETY: as in `setup_hook`; the caller's contract covers `vtable`/`index`.
        let vt = match unsafe { self.vtable() } {
            Some(vt) => vt,
            None => {
                missing_slot("SetupVirtualHook (no IKHook)");
                return hook::INVALID_HOOK;
            }
        };
        let Some(f) = vt.SetupVirtualHook else {
            missing_slot("SetupVirtualHook");
            return hook::INVALID_HOOK;
        };
        // SAFETY: as above.
        unsafe {
            f(
                self.target(),
                vtable,
                index,
                context,
                removed_function,
                pre,
                post,
                make_return,
                make_call_original,
                stack_size,
                r#async,
            )
        }
    }

    /// `IKHook::RemoveHook` (`khook.hpp:2188`). `hook_removal_fn` is `void (*)(HookID_t,
    /// void*)` in the header; the trait carries it as an untyped pointer, which is the same
    /// address.
    unsafe fn remove_hook(
        &self,
        id: HookId,
        r#async: bool,
        hook_removal_fn: *mut c_void,
        context: *mut c_void,
    ) {
        // SAFETY: as in `setup_hook`; the caller's contract covers `id`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            missing_slot("RemoveHook (no IKHook)");
            return;
        };
        let Some(f) = vt.RemoveHook else {
            missing_slot("RemoveHook");
            return;
        };
        // SAFETY: as above; the generated slot declares `*const c_void` for the callback.
        unsafe {
            f(
                self.target(),
                id,
                r#async,
                hook_removal_fn.cast_const(),
                context,
            )
        };
    }

    /// `IKHook::GetContextPtr` (`khook.hpp:2189`).
    fn get_context_ptr(&self) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.GetContextPtr {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => {
                missing_slot("GetContextPtr");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::GetOriginalFunction` (`khook.hpp:2190`).
    fn get_original_function(&self) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.GetOriginalFunction {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => {
                missing_slot("GetOriginalFunction");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::GetOriginalValuePtr` (`khook.hpp:2191`).
    fn get_original_value_ptr(&self) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.GetOriginalValuePtr {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => {
                missing_slot("GetOriginalValuePtr");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::GetOverrideValuePtr` (`khook.hpp:2192`).
    fn get_override_value_ptr(&self) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.GetOverrideValuePtr {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => {
                missing_slot("GetOverrideValuePtr");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::GetCurrentValuePtr` (`khook.hpp:2193`); `pop` is passed through.
    fn get_current_value_ptr(&self, pop: bool) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.GetCurrentValuePtr {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target(), pop) },
            None => {
                missing_slot("GetCurrentValuePtr");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::DestroyReturnValue` (`khook.hpp:2194`).
    fn destroy_return_value(&self) {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return;
        };
        match vt.DestroyReturnValue {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => missing_slot("DestroyReturnValue"),
        }
    }

    /// `IKHook::FindOriginal` (`khook.hpp:2195`).
    fn find_original(&self, function: *mut c_void) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.FindOriginal {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target(), function) },
            None => {
                missing_slot("FindOriginal");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::FindOriginalVirtual` (`khook.hpp:2196`).
    fn find_original_virtual(&self, vtable: *mut *mut c_void, index: i32) -> *mut c_void {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.FindOriginalVirtual {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target(), vtable, index) },
            None => {
                missing_slot("FindOriginalVirtual");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::DoRecall` (`khook.hpp:2197`); `action` is passed as the header's `u8`-sized
    /// `KHook::Action` (`khook.hpp:41`, the trait's `Action` is `#[repr(u8)]`).
    unsafe fn do_recall(
        &self,
        action: Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        init_op: *mut c_void,
        deinit_op: *mut c_void,
    ) -> *mut c_void {
        // SAFETY: as in `setup_hook`; the caller's contract covers the value buffers.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        let Some(f) = vt.DoRecall else {
            missing_slot("DoRecall");
            return ptr::null_mut();
        };
        // SAFETY: as above.
        unsafe {
            f(
                self.target(),
                action as u8,
                ptr_to_return,
                return_size,
                init_op,
                deinit_op,
            )
        }
    }

    /// `IKHook::SaveReturnValue` (`khook.hpp:2198`); `original` is passed through, because
    /// KHook keys its per-call value snapshots on it (`detour.cpp:373-404`).
    #[allow(clippy::too_many_arguments)]
    unsafe fn save_return_value(
        &self,
        action: Action,
        ptr_to_return: *mut c_void,
        return_size: usize,
        init_op: *mut c_void,
        deinit_op: *mut c_void,
        original: bool,
    ) {
        // SAFETY: as in `setup_hook`; the caller's contract covers the value buffers.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return;
        };
        let Some(f) = vt.SaveReturnValue else {
            missing_slot("SaveReturnValue");
            return;
        };
        // SAFETY: as above.
        unsafe {
            f(
                self.target(),
                action as u8,
                ptr_to_return,
                return_size,
                init_op,
                deinit_op,
                original,
            )
        };
    }

    /// `IKHook::LookupSignature` (`khook.hpp:2199`).
    fn lookup_signature(
        &self,
        start: *mut c_void,
        size: usize,
        signature: *const core::ffi::c_char,
    ) -> *mut c_void {
        // SAFETY: as in `setup_hook`; the caller's contract covers the search range and string.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return ptr::null_mut();
        };
        match vt.LookupSignature {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target(), start, size, signature) },
            None => {
                missing_slot("LookupSignature");
                ptr::null_mut()
            }
        }
    }

    /// `IKHook::WasOriginalFunctionSkipped` (`khook.hpp:2200`).
    fn was_original_function_skipped(&self) -> bool {
        // SAFETY: as in `setup_hook`.
        let Some(vt) = (unsafe { self.vtable() }) else {
            return false;
        };
        match vt.WasOriginalFunctionSkipped {
            // SAFETY: as above.
            Some(f) => unsafe { f(self.target()) },
            None => {
                missing_slot("WasOriginalFunctionSkipped");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A detached adapter has no object: every getter answers null and every setup answers
    /// `INVALID_HOOK` instead of dereferencing a null `this`.
    #[test]
    fn detached_adapter_answers_the_neutral_values() {
        let api = VtableKhook::new();
        assert!(api.get_context_ptr().is_null());
        assert!(api.get_original_function().is_null());
        assert!(api.get_original_value_ptr().is_null());
        assert!(api.get_override_value_ptr().is_null());
        assert!(api.get_current_value_ptr(false).is_null());
        assert!(api.find_original(ptr::null_mut()).is_null());
        assert!(api.find_original_virtual(ptr::null_mut(), 0).is_null());
        assert!(api
            .lookup_signature(ptr::null_mut(), 0, ptr::null())
            .is_null());
        assert!(!api.was_original_function_skipped());
        // SAFETY: a detached adapter dereferences nothing, which is the case under test.
        unsafe {
            assert_eq!(
                api.setup_hook(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    true,
                ),
                hook::INVALID_HOOK
            );
        }
    }
}
