//! The bridge's wiring of the `KHookApi` over Metamod's generated `IKHook_vtable`.
//!
//! The adapter itself - [`VtableKhook`], one method per generated virtual, every argument
//! forwarded verbatim - moved to [`s2sdk::khook_vtable`] so a second Metamod plugin can share
//! it (the coexistence fixture, `crates/mm-testplugin`). What stays here is the one
//! bridge-specific fact: this crate's diagnostics (a null `IKHook` slot, the
//! install/uninstall traces) go through [`crate::log`], which is what the adapter's
//! diagnostic sink is pointed at here, before the install that can emit the first of those
//! lines.
//!
//! The behaviour is the adapter's, unchanged: [`install`] stores the `IKHook*` and puts the
//! adapter into `s2sdk::hook`; [`uninstall`] forgets both. See the adapter's module docs for
//! the lifetime contract (`core/metamod.cpp:1081`, `ISmmPlugin.h:107`).

use s2sdk_sys::generated::IKHook;

pub use s2sdk::khook_vtable::VtableKhook;

/// Point the adapter at `obj` and install it as `s2sdk::hook`'s API.
///
/// The adapter's diagnostic sink is pointed at [`crate::log`] here, so a null slot on the
/// object this install accepts - and the install/uninstall traces themselves - land in the
/// bridge's log, prefixed like every other line. The sink is process-wide and never cleared;
/// the bridge's log function is a plain `fn(&str)` that writes nowhere when the log path does
/// not resolve, so an installed sink cannot outlive anything it would dereference.
///
/// # Safety
///
/// `obj` must be the live `IKHook` `ISmmAPI::GetDetourInterface` returned for this plugin, and
/// must stay valid until [`uninstall`] runs.
pub unsafe fn install(obj: *mut IKHook) {
    s2sdk::khook_vtable::set_diagnostic_sink(crate::log::line);
    // SAFETY: the caller's contract covers `obj`; the delegate is this module's only behaviour.
    unsafe { s2sdk::khook_vtable::install(obj) };
}

/// Forget the API. Only safe once no [`s2sdk::hook::Hook`] is alive any more - the plugin's
/// `Unload` runs [`s2sdk::hook::remove_all_blocking`] first for exactly that reason.
pub fn uninstall() {
    s2sdk::khook_vtable::uninstall();
}
