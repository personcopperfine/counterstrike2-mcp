//! Module identity: the Rust port of the C bridge's fail-closed verification
//! (`bridge/mcp_bridge.c:1149-1405`).
//!
//! The C bridge never reads a game module before it has proven the module is exactly the build
//! the pins in `bridge/bridge_pins.h` were measured on: the PE header facts, the CodeView RSDS
//! GUID/age, the file's SHA-256, the prologue bytes of every pinned function and the target of
//! every pinned vtable slot. Only then does it pin the image by address
//! (`GetModuleHandleExW(MODULE_PIN_FLAGS, ...)`) so it cannot be swapped afterwards. This module
//! ports that block over the generated tables in [`crate::generated::pins`].
//!
//! # What is here
//!
//! * [`State`] and [`Modules`]: the per-module state machine
//!   (`not_loaded -> verifying -> verified | mismatch`), [`Modules::poll`] (the throttled
//!   starter, the C's `modules_poll`), the background verifier with the C's order
//!   ([`ORDER`]), [`Modules::verified_mask`] and the caps revision counter.
//! * [`verify_one`]: the check sequence and its log lines, over an abstract image
//!   ([`Image`]: RVA-indexed bytes plus the base the image is mapped at) and the file's SHA-256
//!   ([`FileHash`]), so it runs on the host against synthetic and real images alike.
//! * [`Platform`] and [`Live`]: the thin loader adapter: [`Platform::loaded`] delegates the
//!   module enumeration and [`select_module`] to [`s2sdk::module`] (the one game-module finder),
//!   and [`Live`] answers the mapped image's bytes, `GetModuleFileNameA` + `sha256_file`, and
//!   `GetModuleHandleExW` with the C's flags to pin.
//!
//! # Which image a module name means (Metamod loads a second `server.dll`)
//!
//! The C asked `GetModuleHandleA("server.dll")` (`bridge/mcp_bridge.c:1310`). That answers the
//! **first** module whose base name matches (Microsoft Learn, `GetModuleHandleA`: the name is
//! compared case-independently to the names of the modules mapped in the calling process, and a
//! base name that more than one module carries resolves to the one loaded first). With Metamod
//! installed - which is always, since lane C1 - the process maps two images named `server.dll`:
//! Metamod's loader stub (`<game>\csgo\addons\metamod\bin\win64\server.dll`, loaded first) and
//! the game's own (`<game>\csgo\bin\win64\server.dll`, the one the pins describe). The C's
//! question therefore landed on the stub, the TimeDateStamp check failed, and every `server.dll`
//! row failed closed.
//!
//! This build never asks by base name for verification. [`Platform::loaded`] answers every image
//! of the name, and the selection is [`s2sdk::module::select_module`] - the one game-module
//! finder, shared with `ops::ident::mod_iface` and every other pinned-module lookup: Metamod's
//! directory is excluded outright, the image under the game root is preferred, and a name that
//! is still ambiguous is refused rather than guessed - such a module stays `not_loaded` and
//! never verifies against the wrong image. [`s2sdk::module::Module::find_game`] is the same
//! rules with the image opened, for callers that want the module rather than its pins.
//!
//! # Windows and Linux
//!
//! [`Live`] is the Windows loader. On Linux it reports every Windows module name as
//! `not_loaded`, so [`Modules::poll`] never starts a verifier: this module's pins are the
//! Windows build's ([`crate::generated::pins::PIN_MODULES`]) and the Linux pins are a later
//! work package (`crate::generated::pins::linux` exists but carries no `bridge` module yet).
//! That is a property of the adapter, not a test skip: the host tests drive [`verify_one`] and
//! [`Modules`] through their own [`Platform`].
//!
//! # Numbers
//!
//! Module names, hashes, timestamps, sizes, checksums and RSDS identities are the generated
//! `PIN_MODULES` rows; byte pins are `PIN_BYTES` and slot pins `PIN_SLOTS`. The PE field offsets
//! live in [`crate::pe`] as the `winnt.h` facts the C names. The only literals here are the C's
//! log strings and the `Need*` bit values ([`need_bit`]), which the C spells as `NEED_*`
//! (`bridge/mcp_bridge.c:1030-1043`); the Rust constants for those bits belong to
//! `registry.rs` (WP2.C) and the orchestrator reconciles the two at merge.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Arc, Condvar, Mutex};

// The one game-module finder lives in `s2sdk::module` now (it owns the loader queries); the
// types stay reachable at this module's path because [`Platform`]'s [`loaded`](Platform::loaded)
// answers [`LoadedModule`]s and [`select_module`] is the selection the verifier applies.
pub use s2sdk::module::{LoadedModule, Reject, select_module};

use crate::generated::pins::{
    PIN_BYTES, PIN_CLIENT, PIN_ENGINE2, PIN_HAMMER, PIN_MODELDOC, PIN_MODULE_COUNT, PIN_MODULES,
    PIN_PET, PIN_RSV, PIN_SCENESYSTEM, PIN_SCHEMA, PIN_SERVER, PIN_SOUND, PIN_TF2, PIN_TIER0,
    PIN_SLOTS, PinBytes, PinModule, PinSlot,
};
use crate::{log, pe, registry};

/// The number of pinned modules: `PIN_MODULE_COUNT` (12), the C's `PIN_MODULE_COUNT`
/// (`bridge/mcp_bridge.c:1151` sizes `g_modState`/`g_modBase` with it).
pub const MODULE_COUNT: usize = PIN_MODULE_COUNT;

/// A module's verification state - the C's `MS_*` enum (`bridge/mcp_bridge.c:1149`), same
/// discriminants so a port of the JSON status object can print them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
    /// `MS_NOT_LOADED`: the module has not been seen loaded yet.
    NotLoaded = 0,
    /// `MS_VERIFYING`: a verifier thread is checking it.
    Verifying = 1,
    /// `MS_VERIFIED`: every check passed and the image is pinned.
    Verified = 2,
    /// `MS_MISMATCH`: a check failed; the module is fail-closed and never retried.
    Mismatch = 3,
}

impl State {
    /// The name the C's `mod_state_name` reports (`bridge/mcp_bridge.c:1165-1172`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotLoaded => "not_loaded",
            Self::Verifying => "verifying",
            Self::Verified => "verified",
            Self::Mismatch => "mismatch",
        }
    }

    /// The state for an atomic's integer value; an unknown value stays `not_loaded`, like the
    /// C's `default:` arm.
    const fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Verifying,
            2 => Self::Verified,
            3 => Self::Mismatch,
            _ => Self::NotLoaded,
        }
    }
}

/// The same four states, in the registry's spelling (`registry::ModState` is the JSON-facing
/// enum the reply builders take; `State` is the verifier's).
impl From<State> for registry::ModState {
    fn from(state: State) -> Self {
        match state {
            State::NotLoaded => registry::ModState::NotLoaded,
            State::Verifying => registry::ModState::Verifying,
            State::Verified => registry::ModState::Verified,
            State::Mismatch => registry::ModState::Mismatch,
        }
    }
}

/// The module-state source the capability replies and the dispatch gate read (WP2.D).
///
/// In the game it is [`Modules<Live>`]; a host test injects its own to pin what a reply says for
/// a given state (the `-32017` arm cannot be reached through the live Windows modules on the
/// host). Two questions, because the C asks two: `mod_state_name` reports the raw state, and
/// `mod_ok` (`mcp_bridge.c:1156`) also requires the pinned base.
pub trait ModuleViews: Send + Sync {
    /// The state `mod_state_name` reports (`mcp_bridge.c:1165-1172`).
    fn state(&self, module: u8) -> registry::ModState;

    /// The C's `mod_ok` (`mcp_bridge.c:1156`): verified, based and pinned.
    fn is_verified(&self, module: u8) -> bool;

    /// The C's `g_capsRevision` (`mcp_bridge.c:1153`).
    fn revision(&self) -> u64;

    /// The pins this module's identity check relaxed through KHook (see [`slot_matches`]), in
    /// the order the check met them. The default is the empty list: a state source that does
    /// not track relaxation (every test double before KHook) reports none.
    fn relaxed(&self, _module: u8) -> Vec<&'static str> {
        Vec::new()
    }
}

impl<P: Platform + 'static> ModuleViews for Modules<P> {
    fn state(&self, module: u8) -> registry::ModState {
        Modules::state(self, module)
            .unwrap_or(State::NotLoaded)
            .into()
    }

    fn is_verified(&self, module: u8) -> bool {
        Modules::is_verified(self, module)
    }

    fn revision(&self) -> u64 {
        Modules::caps_revision(self)
    }

    fn relaxed(&self, module: u8) -> Vec<&'static str> {
        Modules::relaxed(self, module)
    }
}

/// Every module's state in pin order, as `control.*` reports it (`mod_state_name`,
/// `mcp_bridge.c:1165-1172`): the raw state, not the mask's `mod_ok` fold.
#[must_use]
pub fn views(source: &dyn ModuleViews) -> [registry::ModuleView; MODULE_COUNT] {
    core::array::from_fn(|module| {
        registry::ModuleView::with_relaxed(
            source.state(module as u8),
            source.relaxed(module as u8),
        )
    })
}

/// The C's `verified_mask` over any [`ModuleViews`] source, through its one implementation
/// ([`registry::verified_mask`]).
///
/// The input folds `mod_ok` (`mcp_bridge.c:1156`) into the state: a `verified` state whose image
/// is not pinned contributes no bit (fail closed as `mismatch`), exactly as the C's mask does.
#[must_use]
pub fn mask(source: &dyn ModuleViews) -> u32 {
    let mask_views: [registry::ModuleView; MODULE_COUNT] = core::array::from_fn(|module| {
        let state = source.state(module as u8);
        let state = if source.is_verified(module as u8) {
            registry::ModState::Verified
        } else if state == registry::ModState::Verified {
            registry::ModState::Mismatch
        } else {
            state
        };
        registry::ModuleView::new(state)
    });
    registry::verified_mask(&mask_views)
}

/// The order the verifier walks the modules in - the C's `order` array
/// (`bridge/mcp_bridge.c:1378`): toolframework2 first because `doc.active` only needs it and its
/// hash is 3x smaller, then the small rising chain, then hammer and the large modules.
///
/// The C's comment is a warning worth keeping: every pin must be listed exactly once - a
/// missing entry leaves a `0` (= `PIN_HAMMER` duplicate) and the module is never verified. The
/// test `order_is_a_permutation` holds this array to that.
pub const ORDER: [u8; MODULE_COUNT] = [
    PIN_TF2, PIN_TIER0, PIN_SCHEMA, PIN_SCENESYSTEM, PIN_ENGINE2, PIN_RSV, PIN_HAMMER, PIN_CLIENT,
    PIN_SERVER, PIN_SOUND, PIN_MODELDOC, PIN_PET,
];

/// The `NEED_*` bit a verified module contributes to [`Modules::verified_mask`], or `0` for an
/// index that is not a module.
///
/// The C's `verified_mask` (`bridge/mcp_bridge.c:1155-1163`) maps module -> bit inline; this is
/// the same mapping as a table so a caller can use it per module. The bit values are the C's
/// `NEED_*` constants:
///
/// | module index | C constant | value | C line |
/// |---|---|---|---|
/// | [`PIN_HAMMER`] (0) | `NEED_HAMMER` | 2 | `bridge/mcp_bridge.c:1031` |
/// | [`PIN_TF2`] (1) | `NEED_TF2` | 1 | `bridge/mcp_bridge.c:1030` |
/// | [`PIN_SCENESYSTEM`] (2) | `NEED_SCENE` | 4 | `bridge/mcp_bridge.c:1032` |
/// | [`PIN_ENGINE2`] (3) | `NEED_ENGINE` | 8 | `bridge/mcp_bridge.c:1033` |
/// | [`PIN_RSV`] (4) | `NEED_RSV` | 16 | `bridge/mcp_bridge.c:1034` |
/// | [`PIN_CLIENT`] (5) | `NEED_CLIENT` | 32 | `bridge/mcp_bridge.c:1036` |
/// | [`PIN_SERVER`] (6) | `NEED_SERVER` | 64 | `bridge/mcp_bridge.c:1038` |
/// | [`PIN_TIER0`] (7) | `NEED_TIER0` | 128 | `bridge/mcp_bridge.c:1039` |
/// | [`PIN_SCHEMA`] (8) | `NEED_SCHEMA` | 256 | `bridge/mcp_bridge.c:1040` |
/// | [`PIN_SOUND`] (9) | `NEED_SOUND` | 512 | `bridge/mcp_bridge.c:1041` |
/// | [`PIN_MODELDOC`] (10) | `NEED_MODELDOC` | 1024 | `bridge/mcp_bridge.c:1042` |
/// | [`PIN_PET`] (11) | `NEED_PET` | 2048 | `bridge/mcp_bridge.c:1043` |
#[must_use]
pub const fn need_bit(module: u8) -> u32 {
    match module {
        PIN_HAMMER => registry::NEED_HAMMER, // mcp_bridge.c:1031
        PIN_TF2 => registry::NEED_TF2, // mcp_bridge.c:1030
        PIN_SCENESYSTEM => registry::NEED_SCENE, // mcp_bridge.c:1032
        PIN_ENGINE2 => registry::NEED_ENGINE, // mcp_bridge.c:1033
        PIN_RSV => registry::NEED_RSV, // mcp_bridge.c:1034
        PIN_CLIENT => registry::NEED_CLIENT, // mcp_bridge.c:1036
        PIN_SERVER => registry::NEED_SERVER, // mcp_bridge.c:1038
        PIN_TIER0 => registry::NEED_TIER0, // mcp_bridge.c:1039
        PIN_SCHEMA => registry::NEED_SCHEMA, // mcp_bridge.c:1040
        PIN_SOUND => registry::NEED_SOUND, // mcp_bridge.c:1041
        PIN_MODELDOC => registry::NEED_MODELDOC, // mcp_bridge.c:1042
        PIN_PET => registry::NEED_PET, // mcp_bridge.c:1043
        _ => 0,
    }
}

/// Why [`verify_one`] failed - one variant per `logline` the C can emit for a check
/// (`bridge/mcp_bridge.c:1304-1370`), in the C's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// Nothing is mapped under the module's file name any more (it unloaded after the poll saw
    /// it, or the walk's selection had nothing to verify).
    NotLoaded,
    /// `pe_info` refused the image (not `MZ`/`PE\0\0`/PE32+).
    PeParse,
    /// `IMAGE_FILE_HEADER::TimeDateStamp` differs.
    TimeDateStamp,
    /// `IMAGE_OPTIONAL_HEADER64::SizeOfImage` differs.
    SizeOfImage,
    /// `IMAGE_OPTIONAL_HEADER64::CheckSum` differs.
    CheckSum,
    /// No RSDS record, or its GUID/age differ.
    Rsds,
    /// `GetModuleFileNameA` failed (or the name did not fit).
    ModuleName,
    /// The file could not be read/hashed (`sha256_file` returned 0).
    FileHash,
    /// The file's SHA-256 differs from `PinModule::sha256`.
    FileMismatch,
    /// A `PIN_BYTES` prologue differs; carries its `what`.
    BytePin(&'static str),
    /// A `PIN_SLOTS` vtable target differs; carries its `what`.
    SlotPin(&'static str),
    /// `GetModuleHandleExW(PIN)` failed (or the module has no byte pin to pin by).
    Pin,
    /// [`Modules::begin_stop`] was called (the plugin is unloading) and the verifier stopped
    /// before its next call into KHook. **Not** a mismatch: nothing was verified.
    Stopped,
}

impl Fail {
    /// The exact first log line the C writes for this failure (`logline` argument).
    #[must_use]
    pub const fn line(self) -> &'static str {
        match self {
            Self::NotLoaded => "verify: module not loaded any more",
            Self::PeParse => "verify: PE parse failed",
            Self::TimeDateStamp => "verify: TimeDateStamp mismatch",
            Self::SizeOfImage => "verify: SizeOfImage mismatch",
            Self::CheckSum => "verify: CheckSum mismatch",
            Self::Rsds => "verify: RSDS GUID/age mismatch",
            Self::ModuleName => "verify: module file name failed",
            Self::FileHash => "verify: SHA-256 of file failed",
            Self::FileMismatch => "verify: file SHA-256 mismatch",
            Self::BytePin(_) => "verify: byte pin mismatch: ",
            Self::SlotPin(_) => "verify: vtable slot mismatch: ",
            Self::Pin => "verify: GetModuleHandleEx(PIN) failed",
            Self::Stopped => "verify: stopped (the plugin is unloading)",
        }
    }

    /// The second log line, for the two checks the C names its pin with
    /// (`logline(pb->what)` / `logline(ps->what)`).
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::BytePin(what) | Self::SlotPin(what) => Some(what),
            _ => None,
        }
    }
}

/// The stopping path: one line, no `FAIL -> fail closed` (nothing was checked and nothing is
/// wrong - the plugin is unloading), and [`Fail::Stopped`] back to the caller.
fn fail_stopped() -> Result<(), Fail> {
    log::line(Fail::Stopped.line());
    Err(Fail::Stopped)
}

/// The C's failure path: the check's line(s), then `verify: FAIL -> fail closed
/// (PrerequisiteFailed)` (`bridge/mcp_bridge.c:1366-1368`), and the reason back to the caller.
fn fail(why: Fail) -> Result<(), Fail> {
    log::line(why.line());
    if let Some(detail) = why.detail() {
        log::line(detail);
    }
    log::line("verify: FAIL -> fail closed (PrerequisiteFailed)");
    Err(why)
}

/// A module image indexed by RVA, as the Windows loader maps it, plus the base it is mapped at.
///
/// `bytes` is `SizeOfImage` long; index `n` is the byte at `base + n`. The base matters only for
/// the vtable-slot check: vtable entries are absolute addresses (relocated by the loader for a
/// live image, written for the preferred base in a file - [`pe::map_image`] returns that base).
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    bytes: &'a [u8],
    base: u64,
}

impl<'a> Image<'a> {
    /// Wrap an RVA-indexed image and the base it is mapped at.
    #[must_use]
    pub const fn new(bytes: &'a [u8], base: u64) -> Self {
        Self { bytes, base }
    }

    /// The RVA-indexed bytes.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The image base (the live load address, or a file image's preferred base).
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// `len` bytes at `rva`, or `None` when that range is outside the image.
    #[must_use]
    pub fn slice(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        let start = usize::try_from(rva).ok()?;
        self.bytes.get(start..start.checked_add(len)?)
    }

    /// The little-endian 64-bit word at `rva` (a vtable entry).
    #[must_use]
    pub fn word(&self, rva: u32) -> Option<u64> {
        let bytes = self.slice(rva, 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }
}

/// What the adapter learned about the module's file, standing in for the C's
/// `GetModuleFileNameA` + `sha256_file` pair (`bridge/mcp_bridge.c:1330-1340`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileHash {
    /// The file was found and hashed.
    Digest([u8; 32]),
    /// `GetModuleFileNameA` returned 0 or the name did not fit: `verify: module file name
    /// failed`.
    NoName,
    /// The file could not be opened/read or hashed: `verify: SHA-256 of file failed`.
    Unreadable,
}

/// One module's pins, borrowed from the generated tables (or from a test's synthetic row).
#[derive(Clone, Copy)]
pub struct Pins<'a> {
    /// The identity row (`PIN_MODULES[m]`).
    pub identity: &'a PinModule,
    /// That module's `PIN_BYTES` rows, in table order.
    pub bytes: &'a [&'a PinBytes],
    /// That module's `PIN_SLOTS` rows, in table order.
    pub slots: &'a [&'a PinSlot],
}

/// One relaxation a KHook-aware identity check made: the pin's `what` and the `relaxed:` log
/// line's text, formatted where the numbers are still known.
///
/// The line is carried rather than written immediately because it may already be on record for
/// this module: only [`Modules::record_relaxed`] can answer that, and it is the one place that
/// writes the line (so a repeated identity check of the same slot logs once - review round 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relaxation {
    /// The pin's `what` (the key [`Modules::record_relaxed`] dedupes on).
    pub what: &'static str,
    /// The `relaxed: ...` line to write the first time this pin is recorded for the module.
    pub line: String,
}

/// Whether one pinned vtable slot still holds its pinned target, allowing for KHook.
///
/// The exact compare is the C's (`mcp_bridge.c:1626`): `vt[slot] == base + target_rva`. KHook
/// writes the class's **shared** vtable entry permanently
/// (`third_party/khook/src/detour.hpp:96-104`, `SetupVirtual`; the entry is never restored) and answers the pre-hook entry through
/// `FindOriginalVirtual` (`detour.cpp:1907-1915`). So when the word does not match, the entry is
/// asked for its original; if that is the pinned target, another plugin (or an earlier arm of
/// ours - the slot stays hooked after a disable) hooked the slot and the pin is still the pin.
///
/// # The limit of this question (KHook cannot tell us more)
///
/// `FindOriginalVirtual` is keyed on the slot **address**, not on the word it holds: it looks
/// `vtable + index` up in `hooks_detour` and answers the saved original whatever is in the
/// entry now (`detour.cpp:1908-1914`). Two consequences follow, and both are pinned by tests:
///
/// * a raw write **on top of** a slot KHook has already hooked passes: KHook still remembers
///   the pre-hook original (the pin), and `IKHook` exposes no way to ask whether the current
///   word is its own thunk (the interface's whole query surface is the two `FindOriginal`
///   calls, `khook.hpp:2195-2196`);
/// * a raw write on a slot KHook never hooked fails closed: `FindOriginalVirtual` then answers
///   the slot's current entry, which is not the pin.
///
/// With no API installed there is nobody to ask and the answer is exactly today's compare.
///
/// The relaxation is recorded in `relaxed`; the caller logs it through
/// [`Modules::record_relaxed`], which writes the line only when the pin is new for the module.
///
/// # Safety
///
/// `vtable.offset(index)` must be a readable `*mut c_void` (`s2sdk::hook::find_original_virtual`'s
/// contract). Callers pass a verified module's vtable (`base + vt_rva`) or, in a test, a live
/// fabricated table.
pub unsafe fn slot_matches(
    vtable: *mut *mut c_void,
    index: i32,
    got: u64,
    want: u64,
    what: &'static str,
    relaxed: &mut Vec<Relaxation>,
) -> bool {
    if got == want {
        return true;
    }
    // SAFETY: the caller guarantees the slot is readable.
    let original = unsafe { s2sdk::hook::find_original_virtual(vtable, index) };
    let Some(original) = original else {
        return false;
    };
    if original as u64 != want {
        return false;
    }
    if !relaxed.iter().any(|r| r.what == what) {
        relaxed.push(Relaxation {
            what,
            line: format!("relaxed: {what} vt[{index}]=0x{got:x} original=0x{want:x}"),
        });
    }
    true
}

/// Whether one byte pin's prologue still matches, allowing for a foreign KHook inline detour.
///
/// KHook patches a function's prologue in place (`third_party/khook/src/detour.hpp:82-94`,
/// `SetupAddress`, through safetyhook) and answers
/// the pre-hook function through `FindOriginal` (`detour.cpp:1897-1905`). When the address has
/// an inline detour that is not ours (`FindOriginal` answers a different address), this pin's
/// prologue bytes are KHook's jump, not the game's function, so the compare is skipped and the
/// relaxation recorded. A pin whose `FindOriginal` is the function itself is compared as usual;
/// so is every pin with no API installed.
///
/// The skip is a real hole in the byte pins: while another plugin has an inline detour on a
/// pinned function, this gate cannot see a game update to that function's prologue (the bytes
/// there are the detour's). The `relaxed` list in the capabilities object is how that hole is
/// made visible; it is not closed here.
fn byte_pin_matches(
    image: &Image<'_>,
    pin: &PinBytes,
    relaxed: &mut Vec<Relaxation>,
) -> bool {
    let address = image.base().wrapping_add(u64::from(pin.rva));
    if let Some(original) = s2sdk::hook::find_original(address as *mut c_void) {
        if original as u64 != address {
            if !relaxed.iter().any(|r| r.what == pin.what) {
                relaxed.push(Relaxation {
                    what: pin.what,
                    line: format!("relaxed: byte pin {} (KHook inline detour)", pin.what),
                });
            }
            return true;
        }
    }
    let len = usize::from(pin.len);
    let Some(found) = image.slice(pin.rva, len) else {
        return false;
    };
    found == &pin.bytes[..len]
}

/// `verify_one` (`bridge/mcp_bridge.c:1304-1370`): every check, in the C's order, with the C's
/// log lines.
///
/// `image` is `None` for the C's `GetModuleHandleA` failure (the module unloaded after the poll
/// saw it); otherwise it is the mapped image. `file` is the adapter's file evidence. `pin_image`
/// is `GetModuleHandleExW(MODULE_PIN_FLAGS, base + rva, &module)` (`bridge/mcp_bridge.c:1352-1359`),
/// called with the RVA of the module's first byte pin - the address the C pins by, because
/// "an RVA is only an address fact inside this image".
///
/// On success the module is pinned and `verify: OK (module pinned)` is logged; on failure the
/// checks stop at the first mismatch (the C's `ok = 0` short-circuits) and the fail-closed line
/// is logged.
///
/// `relaxed` receives the pins the KHook-aware checks relaxed through ([`slot_matches`],
/// [`byte_pin_matches`]), with the line each one logs; the caller reports them to
/// [`Modules::record_relaxed`], which writes the line only for a pin that is new for the module
/// (a re-verification, or an object check that meets the same slot again, does not repeat it).
///
/// For the KHook question, `image.base()` is used as the image's **live** load address
/// (`base + vt_rva` / `base + rva` are handed to KHook, which reads the table entry). That is
/// what the Windows adapter returns (`GetModuleHandleA`'s base). A file image's preferred base
/// (`pe::map_image`'s, the host tests) is not mapped in this process, so it must only be used
/// with no API installed - which is the case for every host test that maps a file.
///
/// `stopping` is read before every pin that may ask KHook a question
/// ([`byte_pin_matches`]/[`slot_matches`] call `FindOriginal*`); a set flag stops the walk with
/// [`Fail::Stopped`] instead of reaching into a KHook API the unload is about to forget
/// ([`Modules::begin_stop`]). Pass `&AtomicBool::new(false)` when there is nothing to stop.
pub fn verify_one(
    pins: Pins<'_>,
    image: Option<&Image<'_>>,
    file: FileHash,
    relaxed: &mut Vec<Relaxation>,
    stopping: &AtomicBool,
    pin_image: impl FnOnce(u32) -> bool,
) -> Result<(), Fail> {
    let identity = pins.identity;
    log::line("verify: module ");
    log::line(identity.name);

    let Some(image) = image else {
        return fail(Fail::NotLoaded);
    };
    let Some(info) = pe::pe_info(image.bytes()) else {
        return fail(Fail::PeParse);
    };
    if info.time_stamp != identity.time_stamp {
        return fail(Fail::TimeDateStamp);
    }
    if info.size_of_image != identity.size_of_image {
        return fail(Fail::SizeOfImage);
    }
    if info.check_sum != identity.check_sum {
        return fail(Fail::CheckSum);
    }
    if info.rsds != Some((identity.rsds_guid, identity.rsds_age)) {
        return fail(Fail::Rsds);
    }
    match file {
        FileHash::Digest(digest) if digest == identity.sha256 => {}
        FileHash::Digest(_) => return fail(Fail::FileMismatch),
        FileHash::NoName => return fail(Fail::ModuleName),
        FileHash::Unreadable => return fail(Fail::FileHash),
    }
    for pin in pins.bytes {
        if stopping.load(Ordering::SeqCst) {
            return fail_stopped();
        }
        if !byte_pin_matches(image, pin, relaxed) {
            return fail(Fail::BytePin(pin.what));
        }
    }
    for pin in pins.slots {
        if stopping.load(Ordering::SeqCst) {
            return fail_stopped();
        }
        // `((void **)(base + vtRva))[slot] != base + targetRva`, with the slot array walk as a
        // wrapping offset so the C's byte arithmetic is reproduced exactly.
        let Some(word) = image.word(
            pin.vt_rva
                .wrapping_add(u32::from(pin.slot).wrapping_mul(8)),
        ) else {
            return fail(Fail::SlotPin(pin.what));
        };
        // The KHook question goes to the LIVE table, not the copied image: KHook's patch is in
        // the mapped vtable (`base + vt_rva`), and `image.word` above is the copy the gate
        // compares against the pin (which is also the live word, unless something wrote it).
        let vtable = image
            .base()
            .wrapping_add(u64::from(pin.vt_rva)) as *mut *mut c_void;
        // SAFETY: `image` is a loaded module's image (`base` is its load address) and
        // `vt_rva + slot * 8` is a pinned vtable entry inside it, so `vtable.offset(slot)` is
        // readable - the same word `image.word` just read from the copy.
        if !unsafe {
            slot_matches(
                vtable,
                i32::from(pin.slot),
                word,
                image.base().wrapping_add(u64::from(pin.target_rva)),
                pin.what,
                relaxed,
            )
        } {
            return fail(Fail::SlotPin(pin.what));
        }
    }
    let first_rva = pins.bytes.first().map_or(0, |pin| pin.rva);
    if first_rva == 0 || !pin_image(first_rva) {
        return fail(Fail::Pin);
    }
    log::line("verify: OK (module pinned)");
    Ok(())
}

/// One module's pinned rows from the generated tables.
#[must_use]
pub fn pins_for(module: u8) -> (Vec<&'static PinBytes>, Vec<&'static PinSlot>) {
    (
        PIN_BYTES.iter().filter(|pin| pin.module == module).collect(),
        PIN_SLOTS.iter().filter(|pin| pin.module == module).collect(),
    )
}

/// The loader operations verification needs - the module's mapped images, the mapped image's
/// bytes, `GetModuleFileNameA` + `sha256_file`, and `GetModuleHandleExW(MODULE_PIN_FLAGS, ...)`.
///
/// Implementations must fail closed: no candidate is the C's "not loaded", a [`FileHash`] that is
/// not a matching digest fails the hash check, and `false` from [`pin`](Self::pin) fails the pin.
pub trait Platform: Send + Sync {
    /// `GetModuleHandleA(name)`: the load base, or `None`.
    ///
    /// This is the single-image answer; [`loaded`](Self::loaded) is the one the verifier uses,
    /// because a base name alone is ambiguous once Metamod is installed (see
    /// [`select_module`]). Kept as the trait's minimal requirement so a host test's double only
    /// has to answer this.
    fn module_base(&self, name: &str) -> Option<u64>;

    /// Every image the loader has mapped whose file name is `name` (case-insensitive), with the
    /// path it was loaded from (`GetModuleFileNameW`). In loader order, like
    /// `K32EnumProcessModules`.
    ///
    /// The default answers the one [`module_base`](Self::module_base) image with no path - what
    /// an adapter that cannot enumerate can offer; the path rules then cannot apply, which is
    /// what a host test with a single fabricated image wants.
    fn loaded(&self, name: &str) -> Vec<LoadedModule> {
        self.module_base(name)
            .map(|base| LoadedModule {
                base,
                path: String::new(),
            })
            .into_iter()
            .collect()
    }

    /// The host executable's full path (`GetModuleFileNameW(NULL)`), the anchor
    /// [`game_root`] is derived from. `None` keeps only the Metamod rule.
    fn host_path(&self) -> Option<String> {
        None
    }

    /// The module's mapped image (`SizeOfImage` bytes, RVA-indexed), or `None` if it cannot be
    /// read (which verification treats as a parse failure of a loaded module).
    fn image(&self, name: &str, base: u64) -> Option<Vec<u8>>;

    /// `GetModuleFileNameA` + `sha256_file` for the module at `base`.
    fn file_hash(&self, name: &str, base: u64) -> FileHash;

    /// `GetModuleHandleExW(MODULE_PIN_FLAGS, base + rva, &module)`: pin the image so it cannot
    /// be unloaded.
    fn pin(&self, name: &str, base: u64, rva: u32) -> bool;
}

/// What [`Modules::poll`] did on this call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    /// The C's `(++g_pollThrottle & 31) != 0`: this call is not a poll tick.
    Throttled,
    /// No `not_loaded` module is loaded, so there is nothing to verify.
    NothingToDo,
    /// The previous verifier is still running (`g_verifierRunning`).
    AlreadyVerifying,
    /// A verifier thread was started.
    Started,
    /// `CreateThread` failed: logged, and the running flag was reset so a later tick retries.
    ThreadFailed,
}

/// The per-process module state: the C's `g_modState`, `g_modBase`, `g_capsRevision`,
/// `g_pollThrottle` and `g_verifierRunning` (`bridge/mcp_bridge.c:1151-1154, 1372`).
///
/// `Modules` is `Arc`-backed so the verifier thread can hold the state without borrowing the
/// caller's stack; clones share the same state.
pub struct Modules<P: Platform> {
    inner: Arc<Inner<P>>,
}

impl<P: Platform> Clone for Modules<P> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<P: Platform + 'static> core::fmt::Debug for Modules<P> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Modules")
            .field("state", &self.states_debug())
            .field("caps_revision", &self.caps_revision())
            .finish()
    }
}

struct Inner<P: Platform> {
    platform: P,
    /// `g_modState`: the [`State`] discriminant per module.
    states: [AtomicU32; MODULE_COUNT],
    /// `g_modBase`: the verified load base, 0 until verification succeeds.
    bases: [AtomicU64; MODULE_COUNT],
    /// `g_capsRevision`, starting at 1 like the C's initializer.
    caps_revision: AtomicU64,
    /// `g_pollThrottle`.
    poll_throttle: AtomicU32,
    /// `g_verifierRunning`.
    verifier_running: AtomicBool,
    /// Set by [`Modules::begin_stop`] (the unload): a running verifier stops before its next call
    /// into KHook, whose API the unload is about to forget. Cleared by
    /// [`Modules::resume_verify`] on a load, or a refused unload.
    verifier_stopping: AtomicBool,
    /// The verifier thread's handle, so the unload can join it: the thread is **not** detached.
    /// Metamod `dlclose`s this image once `Unload` answers true, and a verifier still executing
    /// its exit path in that image is exactly the crash the join prevents.
    verifier: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Signalled when the verifier clears [`Inner::verifier_running`]: [`Modules::wait_verifier`]
    /// blocks on it instead of polling the flag.
    verifier_done: Condvar,
    /// The mutex [`Inner::verifier_done`] pairs with. It holds no data - the flag is the state -
    /// so a waiter re-checks the flag after every wakeup.
    verifier_done_lock: Mutex<()>,
    /// [`slot_matches`]'s relaxations per module: the pins the module status object reports as
    /// `"relaxed"`. Written by the verifier (and by an object identity check on the main
    /// thread, which can meet a KHook the verifier did not), read by the capabilities builder.
    relaxed: [Mutex<Vec<&'static str>>; MODULE_COUNT],
    /// Set once a module's mapped images were refused ([`Reject`]): the walk writes the one
    /// refusal line per module per process, not once per tick.
    refused: [AtomicBool; MODULE_COUNT],
}

impl<P: Platform + 'static> Modules<P> {
    /// New state with every module `not_loaded` and the caps revision at 1
    /// (`bridge/mcp_bridge.c:1153`).
    #[must_use]
    pub fn new(platform: P) -> Self {
        Self {
            inner: Arc::new(Inner {
                platform,
                states: [const { AtomicU32::new(State::NotLoaded as u32) }; MODULE_COUNT],
                bases: [const { AtomicU64::new(0) }; MODULE_COUNT],
                caps_revision: AtomicU64::new(1),
                poll_throttle: AtomicU32::new(0),
                verifier_running: AtomicBool::new(false),
                verifier_stopping: AtomicBool::new(false),
                verifier: Mutex::new(None),
                verifier_done: Condvar::new(),
                verifier_done_lock: Mutex::new(()),
                relaxed: [const { Mutex::new(Vec::new()) }; MODULE_COUNT],
                refused: [const { AtomicBool::new(false) }; MODULE_COUNT],
            }),
        }
    }

    /// The module's state, or `None` for an index that is not a pin.
    #[must_use]
    pub fn state(&self, module: u8) -> Option<State> {
        let raw = self
            .inner
            .states
            .get(usize::from(module))?
            .load(Ordering::SeqCst);
        Some(State::from_raw(raw))
    }

    /// The C's `mod_state_name` (`bridge/mcp_bridge.c:1165-1172`).
    #[must_use]
    pub fn state_name(&self, module: u8) -> &'static str {
        self.state(module).unwrap_or(State::NotLoaded).name()
    }

    /// The C's `mod_ok`: verified and still holding a base (`bridge/mcp_bridge.c:1156`).
    #[must_use]
    pub fn is_verified(&self, module: u8) -> bool {
        self.state(module) == Some(State::Verified)
            && self
                .inner
                .bases
                .get(usize::from(module))
                .is_some_and(|base| base.load(Ordering::SeqCst) != 0)
    }

    /// The module's verified base, or 0.
    #[must_use]
    pub fn base(&self, module: u8) -> u64 {
        self.inner
            .bases
            .get(usize::from(module))
            .map_or(0, |base| base.load(Ordering::SeqCst))
    }

    /// The C's `verified_mask` (`bridge/mcp_bridge.c:1157-1164`): one `NEED_*` bit per module
    /// the C's `mod_ok` accepts. [`mask`] is the one input-building path and
    /// [`registry::verified_mask`] the one mask computation.
    #[must_use]
    pub fn verified_mask(&self) -> u32 {
        mask(self)
    }

    /// Every module's state in pin order, as `control.*` reports it (`mod_state_name`,
    /// `bridge/mcp_bridge.c:1165-1172`): the raw state, not the mask's `mod_ok` fold.
    #[must_use]
    pub fn views(&self) -> [registry::ModuleView; MODULE_COUNT] {
        views(self)
    }

    /// The C's `g_capsRevision`: 1 at start, incremented after each module's verification.
    #[must_use]
    pub fn caps_revision(&self) -> u64 {
        self.inner.caps_revision.load(Ordering::SeqCst)
    }

    /// `caps_bump` (`bridge/mcp_bridge.c:1173`): tell a capabilities reader the state moved.
    pub fn bump_caps(&self) {
        self.inner.caps_revision.fetch_add(1, Ordering::SeqCst);
    }

    /// The pins this module's identity checks relaxed through KHook ([`slot_matches`]), in the
    /// order they were met. Empty when nothing was relaxed.
    #[must_use]
    pub fn relaxed(&self, module: u8) -> Vec<&'static str> {
        self.inner
            .relaxed
            .get(usize::from(module))
            .map(|list| {
                list.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default()
    }

    /// Record one relaxation for `module` (idempotent: the same pin is listed once) and write its
    /// `relaxed:` line **only when this call inserted it**.
    ///
    /// Returns whether the pin was new for this module. Called by the verifier with the list
    /// [`verify_one`] built, and by [`crate::ops::ident`] when an object identity check meets a
    /// slot KHook moved after the module was verified - which is why the once-per-pin rule lives
    /// here and not in [`slot_matches`]: a slot check runs on every object identity check, and
    /// the line belongs to the first observation, not to the hundredth.
    ///
    /// A new pin also bumps the caps revision: it changes what the capabilities object reports
    /// for this module (`"relaxed"`), and a client that caches capabilities by revision must
    /// re-fetch.
    pub fn record_relaxed(&self, module: u8, relaxation: &Relaxation) -> bool {
        let Some(list) = self.inner.relaxed.get(usize::from(module)) else {
            return false;
        };
        let mut list = list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if list.contains(&relaxation.what) {
            return false;
        }
        list.push(relaxation.what);
        drop(list);
        self.bump_caps();
        true
    }

    /// Replace this module's relaxation list with `relaxed` (the verifier's outcome), bumping the
    /// caps revision when the reported list actually changed.
    fn set_relaxed(&self, module: u8, relaxed: Vec<&'static str>) {
        let Some(list) = self.inner.relaxed.get(usize::from(module)) else {
            return;
        };
        let mut list = list.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *list == relaxed {
            return;
        }
        *list = relaxed;
        drop(list);
        self.bump_caps();
    }

    /// Whether a verifier thread holds the running flag.
    #[must_use]
    pub fn verifier_running(&self) -> bool {
        self.inner.verifier_running.load(Ordering::SeqCst)
    }

    /// Ask a running verifier to stop before its next call into KHook.
    ///
    /// Called by [`crate::plugin::unload`] before anything is torn down: on a forced unload
    /// (`UnloadAll`, `meta clear`, `meta force_unload`, `meta retry`) Metamod ignores the
    /// unload's `false` and `dlclose`s this image, so the verifier must stop on its own - nobody
    /// will wait for it and nobody can stop it afterwards. A verifier that notices the flag
    /// returns through [`Fail::Stopped`] and its module stays `not_loaded`, so it is verified
    /// again by a later tick after a reload.
    ///
    /// The flag stays set on an unload that returns false: the unload cannot tell a forced
    /// unload from an ordinary one, and a forced one gets no second chance to stop the thread.
    /// [`crate::wiring::tick`] clears it ([`Modules::resume_verify`]): a tick proves the image is
    /// still mapped and still ours. A `Load` clears it too. In an mm-only image a refused unload
    /// has already dropped the GameFrame hook, so no tick follows: the plugin stays inert until
    /// the `meta unload` retry succeeds or the process restarts.
    pub fn begin_stop(&self) {
        self.inner.verifier_stopping.store(true, Ordering::SeqCst);
    }

    /// Undo [`Modules::begin_stop`]: a load, or the tick after a refused unload, must let the
    /// verifier run again.
    pub fn resume_verify(&self) {
        self.inner.verifier_stopping.store(false, Ordering::SeqCst);
    }

    /// Whether [`Modules::begin_stop`] was called and not undone.
    #[must_use]
    pub fn verifier_stopping(&self) -> bool {
        self.inner.verifier_stopping.load(Ordering::SeqCst)
    }

    /// Join the verifier thread, if one was started.
    ///
    /// **Only after a successful [`Modules::wait_verifier`]**: the wait proves the verifier's
    /// body finished (its [`RunningGuard`] clears the flag at the end of `verify_all`), so the
    /// join returns as soon as the thread's own exit path completes. Joining a verifier the wait
    /// timed out on would block for as long as the verifier does, which is the one thing the
    /// unload deadline exists to prevent - hence the caller's order (wait, then join).
    ///
    /// Returns whether a verifier was joined and did not panic (`true` when there was none).
    #[must_use]
    pub fn join_verifier(&self) -> bool {
        let handle = self
            .inner
            .verifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match handle {
            None => true,
            Some(handle) => handle.join().is_ok(),
        }
    }

    /// Block until no verifier thread runs, or `deadline` passes; whether it went idle.
    ///
    /// The wait is signalled by the verifier clearing the running flag
    /// ([`RunningGuard`](struct@RunningGuard) notifies [`Inner::verifier_done`]), never a poll
    /// loop: `Unload` calls this before it forgets the `IKHook` (`crate::plugin::unload`), and a
    /// verifier still inside `verify_one` holds a `&'static dyn KHookApi` it got from
    /// [`s2sdk::hook::api`] - forgetting the API under it would leave that reference dangling
    /// while Metamod deletes its `KHookImpl` (`core/metamod_plugins.cpp:59`).
    ///
    /// A zero deadline asks once and does not wait. The flag is checked while holding the
    /// signal's mutex, so a verifier that finishes between the check and the wait cannot be
    /// missed (the guard clears the flag, then locks and notifies).
    #[must_use]
    pub fn wait_verifier(&self, deadline: Duration) -> bool {
        if !self.verifier_running() {
            return true;
        }
        let until = std::time::Instant::now() + deadline;
        let mut guard = self
            .inner
            .verifier_done_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if !self.verifier_running() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= until {
                return !self.verifier_running();
            }
            let (next, _timeout) = self
                .inner
                .verifier_done
                .wait_timeout(guard, until - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard = next;
        }
    }

    /// The C's `modules_poll` (`bridge/mcp_bridge.c:1392-1405`): every 32nd call, if some
    /// `not_loaded` module is now loaded, start the verifier unless one already runs.
    ///
    /// The C increments the throttle before any other test, so calls 1-31 return
    /// [`Poll::Throttled`]; this does the same with `fetch_add` and tests the new value.
    pub fn poll(&self) -> Poll {
        let tick = self.inner.poll_throttle.fetch_add(1, Ordering::SeqCst);
        if tick.wrapping_add(1) & (POLL_PERIOD - 1) != 0 {
            return Poll::Throttled;
        }
        let need = (0..MODULE_COUNT as u8).any(|module| {
            self.state(module) == Some(State::NotLoaded)
                // The cheap question first (`GetModuleHandleA`), so a module this host never
                // maps costs one loader call instead of a process-wide enumeration; then the
                // real one, which also skips a module whose only image is Metamod's stub.
                && self
                    .inner
                    .platform
                    .module_base(PIN_MODULES[usize::from(module)].name)
                    .is_some()
                && self.choose(module).is_ok()
        });
        if !need {
            return Poll::NothingToDo;
        }
        if self
            .inner
            .verifier_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Poll::AlreadyVerifying;
        }
        // The previous verifier is finished (the CAS above only succeeded because its flag is
        // clear): take its handle out before replacing it, so no finished thread of ours is left
        // detached when the plugin unloads. `join` returns immediately here.
        if let Some(previous) = self
            .inner
            .verifier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = previous.join();
        }
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("cs2-bridge-verify".into())
            .spawn(move || verify_all(&inner));
        match spawned {
            Ok(handle) => {
                // Not detached any more: the unload joins it ([`Modules::join_verifier`]).
                *self
                    .inner
                    .verifier
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
                Poll::Started
            }
            Err(_) => {
                log::line("verify: CreateThread failed");
                self.inner.verifier_stopped();
                Poll::ThreadFailed
            }
        }
    }

    /// The C's `verify_all_thread` (`bridge/mcp_bridge.c:1373-1390`) body, synchronously: walk
    /// [`ORDER`], and for every `not_loaded` module that is loaded, mark it `verifying`, run
    /// [`verify_one`], mark it `verified`/`mismatch`, and bump the caps revision.
    ///
    /// [`poll`](Self::poll) runs this on a thread; the tests call it directly when they want a
    /// deterministic result, and it is the same code either way. Calling it directly while a
    /// verifier thread runs is a caller error: `poll` is the only entry point that arbitrates
    /// the single-verifier rule.
    pub fn verify_all(&self) {
        verify_all(&self.inner);
    }

    /// Snapshot for [`Debug`].
    fn states_debug(&self) -> Vec<(&'static str, State, u64)> {
        (0..MODULE_COUNT as u8)
            .map(|module| {
                (
                    PIN_MODULES[usize::from(module)].name,
                    self.state(module).unwrap_or(State::NotLoaded),
                    self.base(module),
                )
            })
            .collect()
    }

    /// The image a pin names, chosen from everything the loader has mapped under its file name
    /// ([`select_module`]): the game's own image, never Metamod's loader stub, and never a
    /// guess between two candidates.
    fn choose(&self, module: u8) -> Result<LoadedModule, NoImage> {
        let name = PIN_MODULES[usize::from(module)].name;
        let candidates = self.inner.platform.loaded(name);
        let host = self.inner.platform.host_path();
        match select_module(name, host.as_deref(), &candidates) {
            Ok(Some(chosen)) => Ok(chosen.clone()),
            Ok(None) => Err(NoImage::NotLoaded),
            Err(reject) => Err(NoImage::Refused(reject)),
        }
    }

    /// Verify one module end to end: resolve it through the adapter and run [`verify_one`].
    /// On success, returns the base the checks saw (the C's `g_modBase[m] = h`,
    /// `bridge/mcp_bridge.c:1361`).
    fn verify_module(&self, module: u8) -> Result<u64, Fail> {
        let identity = &PIN_MODULES[usize::from(module)];
        let (byte_pins, slot_pins) = pins_for(module);
        let pins = Pins {
            identity,
            bytes: &byte_pins,
            slots: &slot_pins,
        };

        // The one image this pin is about: the game's own, not Metamod's loader stub (the walk
        // has already skipped a module with nothing usable, so a refusal here is the same
        // "nothing to verify" answer as an unmapped module).
        let base = match self.choose(module) {
            Ok(chosen) => chosen.base,
            Err(_) => return Err(Fail::NotLoaded),
        };
        let image_bytes = self.inner.platform.image(identity.name, base);
        let image = image_bytes
            .as_deref()
            .map(|bytes| Image::new(bytes, base));
        let file = self.inner.platform.file_hash(identity.name, base);
        let mut relaxed = Vec::new();
        let outcome = verify_one(
            pins,
            image.as_ref(),
            file,
            &mut relaxed,
            &self.inner.verifier_stopping,
            |rva| self.inner.platform.pin(identity.name, base, rva),
        );
        // Recorded whether the module verified or failed: a relaxation the check made before a
        // later mismatch is still a fact an operator needs to see. Each line is written only
        // when its pin is new for this module (`record_relaxed`), so a re-verification that
        // relaxes the same slot again does not repeat it.
        for relaxation in &relaxed {
            if self.record_relaxed(module, relaxation) {
                log::line(&relaxation.line);
            }
        }
        self.set_relaxed(module, relaxed.iter().map(|r| r.what).collect());
        outcome.map(|()| base)
    }
}

/// Clears `g_verifierRunning` on every exit path, including a panic in a verifier thread, and
/// wakes [`Modules::wait_verifier`].
struct RunningGuard<'a, P: Platform>(&'a Inner<P>);

impl<P: Platform> Drop for RunningGuard<'_, P> {
    fn drop(&mut self) {
        self.0.verifier_stopped();
    }
}

impl<P: Platform> Inner<P> {
    /// Clear the running flag and signal the waiters. The flag is set before the mutex is taken,
    /// so a waiter that holds (or is about to take) the signal's mutex either sees the cleared
    /// flag or gets the notification.
    fn verifier_stopped(&self) {
        self.verifier_running.store(false, Ordering::SeqCst);
        let _guard = self
            .verifier_done_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.verifier_done.notify_all();
    }
}

/// [`Modules::choose`]'s answer when there is no image to verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoImage {
    /// Nothing is mapped under the name: the C's "not loaded", retried on a later tick.
    NotLoaded,
    /// Something is mapped but refused ([`Reject`]): also retried, and the one line is written
    /// the first time the walk meets it.
    Refused(Reject),
}

fn verify_all<P: Platform + 'static>(inner: &Arc<Inner<P>>) {
    let _running = RunningGuard(inner);
    'modules: for module in ORDER {
        // The unload's stopping flag: do not begin another module's checks once the plugin is
        // going away - each one asks KHook `FindOriginal*`, and the unload forgets that API.
        if inner.verifier_stopping.load(Ordering::SeqCst) {
            log::line(Fail::Stopped.line());
            break;
        }
        let index = usize::from(module);
        if State::from_raw(inner.states[index].load(Ordering::SeqCst)) != State::NotLoaded {
            continue;
        }
        // Which image this pin names, before anything is verified: nothing mapped means the C's
        // "not loaded" (retry on a later tick), and a refusal means the images that are mapped
        // are not the game's - also retried, with one line the first time it is seen.
        let modules = Modules {
            inner: Arc::clone(inner),
        };
        match modules.choose(module) {
            Ok(_) => {}
            Err(NoImage::NotLoaded) => continue,
            Err(NoImage::Refused(reject)) => {
                if !inner.refused[index].swap(true, Ordering::SeqCst) {
                    log::line(&format!(
                        "verify: {}: {}",
                        PIN_MODULES[index].name,
                        reject.line()
                    ));
                }
                continue;
            }
        }
        inner.states[index].store(State::Verifying as u32, Ordering::SeqCst);
        let (state, base) = match modules.verify_module(module) {
            Ok(base) => (State::Verified, base),
            Err(Fail::Stopped) => {
                // Nothing was verified: this module stays `not_loaded` so a tick after a reload
                // tries again, and the walk stops here.
                inner.states[index].store(State::NotLoaded as u32, Ordering::SeqCst);
                break 'modules;
            }
            Err(_) => (State::Mismatch, 0),
        };
        if base != 0 {
            inner.bases[index].store(base, Ordering::SeqCst);
        }
        inner.states[index].store(state as u32, Ordering::SeqCst);
        inner.caps_revision.fetch_add(1, Ordering::SeqCst);
    }
}

/// The live loader adapter: the C's `GetModuleHandleA` / `GetModuleFileNameA` /
/// `GetModuleHandleExW(MODULE_PIN_FLAGS, ...)` (`bridge/mcp_bridge.c:1310, 1331, 1358`) plus a
/// reading of the mapped image.
///
/// On Windows the image is copied out section by section (headers, then each section's
/// `VirtualSize` at its `VirtualAddress`) because [`verify_one`] wants one RVA-indexed slice and
/// the gaps between sections are not part of the image. On Linux every Windows module name is
/// `not_loaded` - see the module docs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Live;

/// The live module state for this process, built on first use.
#[must_use]
pub fn live() -> &'static Modules<Live> {
    static LIVE: std::sync::OnceLock<Modules<Live>> = std::sync::OnceLock::new();
    LIVE.get_or_init(|| Modules::new(Live))
}

#[cfg(windows)]
mod windows {
    use std::ffi::CString;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleFileNameA,
        GetModuleHandleA, GetModuleHandleExW,
    };

    use super::{FileHash, Live, LoadedModule, Platform};

    /// `MODULE_PIN_FLAGS` (`bridge/mcp_bridge.c:132`): pin by address, so the module named by
    /// an address inside it is pinned (`GET_MODULE_HANDLE_EX_FLAG_PIN`) and addressed rather
    /// than named (`GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS`). Both are `winnt.h` bits.
    const MODULE_PIN_FLAGS: u32 =
        GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS;

    /// The C's `char path[300]` (`bridge/mcp_bridge.c:1330`): a longer name is a failure, not a
    /// truncation.
    const MODULE_PATH_BYTES: usize = 300;

    /// The C's `ReadFile` chunk (`bridge/mcp_bridge.c:1294`): 256 KiB.
    const HASH_CHUNK: usize = 1 << 18;

    impl Platform for Live {
        fn module_base(&self, name: &str) -> Option<u64> {
            let name = CString::new(name).ok()?;
            // SAFETY: `name` is a NUL-terminated string that outlives the call;
            // `GetModuleHandleA` only reads it (`bridge/mcp_bridge.c:1310`).
            let handle = unsafe { GetModuleHandleA(name.as_ptr().cast()) };
            // HMODULE is a pointer type on this target (`winnt.h`).
            (!handle.is_null()).then_some(handle as usize as u64)
        }

        /// Every mapped image whose file name is `name`: [`s2sdk::module::mapped`], the one
        /// enumeration (Windows `K32EnumProcessModules` + `GetModuleFileNameW`, Linux every
        /// `dl_iterate_phdr` match).
        ///
        /// This is what makes the verifier immune to Metamod's loader stub: two images are named
        /// `server.dll` in a Metamod host, and `GetModuleHandleA` would answer the stub (the
        /// first one loaded) - [`s2sdk::module::select_module`] picks the game's instead. An
        /// adapter that cannot enumerate answers nothing here, and the walk then treats the
        /// module as not loaded.
        fn loaded(&self, name: &str) -> Vec<LoadedModule> {
            s2sdk::module::mapped(name)
        }

        /// The host executable's full path (`GetModuleFileNameW(NULL)`), the game root's anchor
        /// ([`s2sdk::module::game_root`]).
        fn host_path(&self) -> Option<String> {
            s2sdk::module::host_executable()
        }

        fn image(&self, _name: &str, base: u64) -> Option<Vec<u8>> {
            // SAFETY: `base` is the non-null load address `choose` took from the loader's module
            // list ([`s2sdk::module::select_module`]), so it is a mapped PE image and the header
            // and section table are readable at it (review N4: this used to say
            // `GetModuleHandleA`, which is no longer how the image is found).
            unsafe { image_from_memory(base as *const u8) }
        }

        fn file_hash(&self, _name: &str, base: u64) -> FileHash {
            let mut path = [0u8; MODULE_PATH_BYTES];
            // SAFETY: the handle is a loaded module and `path` is a writable buffer of the
            // declared length (`GetModuleFileNameA`, `bridge/mcp_bridge.c:1331`).
            let len = unsafe {
                GetModuleFileNameA(
                    base as usize as HMODULE,
                    path.as_mut_ptr().cast(),
                    MODULE_PATH_BYTES as u32,
                )
            };
            if len == 0 {
                return FileHash::NoName;
            }
            let Ok(len) = usize::try_from(len) else {
                return FileHash::NoName;
            };
            if len >= MODULE_PATH_BYTES {
                return FileHash::NoName;
            }
            // The ANSI name is read as UTF-8: Wine's paths are ASCII, and a lossy conversion
            // can only make the open fail, which fails the module closed rather than open.
            let name = String::from_utf8_lossy(&path[..len]);
            let path = PathBuf::from(name.as_ref());
            let mut file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(_) => return FileHash::Unreadable,
            };
            let mut hash = crate::pe::Sha256::new();
            let mut chunk = vec![0u8; HASH_CHUNK];
            loop {
                use std::io::Read;
                match file.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => hash.update(&chunk[..n]),
                    Err(_) => return FileHash::Unreadable,
                }
            }
            FileHash::Digest(hash.finish())
        }

        fn pin(&self, _name: &str, base: u64, rva: u32) -> bool {
            let Some(address) = base.checked_add(u64::from(rva)) else {
                return false;
            };
            let mut module: HMODULE = core::ptr::null_mut();
            // SAFETY: `address` is inside the loaded image named by `base` (an RVA from its
            // pins), and `module` is a writable HMODULE (`bridge/mcp_bridge.c:1358`).
            let pinned = unsafe {
                GetModuleHandleExW(
                    MODULE_PIN_FLAGS,
                    address as usize as *const u16,
                    &mut module,
                )
            };
            pinned != 0
        }
    }

    /// Read a `u32` out of the mapped image at `base + offset`.
    ///
    /// # Safety
    ///
    /// `offset` must be inside the mapped image's headers (they are committed as soon as the
    /// module is loaded), which every caller here derives from the `MZ`/`PE` header itself.
    unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
        // SAFETY: the caller guarantees the range is mapped; `read_unaligned` accepts the
        // 4-byte alignment the PE header does not promise.
        unsafe { core::ptr::read_unaligned(base.add(offset).cast::<u32>()) }
    }

    /// The module's mapped image, RVA-indexed, copied out of the live mapping.
    ///
    /// # Safety
    ///
    /// `base` must be the load address of a mapped PE32+ image that stays loaded for the call
    /// (it comes from `GetModuleHandleA`).
    unsafe fn image_from_memory(base: *const u8) -> Option<Vec<u8>> {
        // SAFETY: the DOS header is the first page of every loaded image; `e_lfanew` and the
        // fields below are in the header block the loader itself read from.
        let pe = usize::try_from(unsafe { read_u32(base, crate::pe::PE_E_LFANEW_OFF) }).ok()?;
        if unsafe { read_u32(base, pe) } != crate::pe::PE_SIG {
            return None;
        }
        let optional = pe.checked_add(crate::pe::PE_OPTIONAL_HDR_OFF)?;
        // SAFETY: the optional header is part of the same header block.
        if unsafe { core::ptr::read_unaligned(base.add(optional).cast::<u16>()) }
            != crate::pe::PE32PLUS_MAGIC
        {
            return None;
        }
        // SAFETY: all four are `IMAGE_OPTIONAL_HEADER64` / `IMAGE_FILE_HEADER` fields in the
        // header block.
        let size_of_image = usize::try_from(unsafe {
            read_u32(base, optional + crate::pe::PE32PLUS_SIZEOFIMAGE)
        })
        .ok()?;
        let size_of_headers = usize::try_from(unsafe {
            read_u32(base, optional + crate::pe::PE32PLUS_SIZEOFHEADERS)
        })
        .ok()?;
        let sections =
            usize::from(unsafe { core::ptr::read_unaligned(base.add(pe + crate::pe::PE_NUMBER_OF_SECTIONS_OFF).cast::<u16>()) });
        let optional_size = usize::from(unsafe {
            core::ptr::read_unaligned(
                base.add(pe + crate::pe::PE_SIZE_OF_OPTIONAL_HEADER_OFF)
                    .cast::<u16>(),
            )
        });
        if size_of_headers > size_of_image {
            return None;
        }
        let mut bytes = vec![0u8; size_of_image];
        // SAFETY: the header block is committed and `size_of_headers` bytes long.
        bytes
            .get_mut(..size_of_headers)?
            .copy_from_slice(unsafe { core::slice::from_raw_parts(base, size_of_headers) });

        let table = optional.checked_add(optional_size)?;
        for index in 0..sections {
            let header = table.checked_add(index.checked_mul(crate::pe::PE_SECTION_HEADER_BYTES)?)?;
            // SAFETY: `header` is the index-th `IMAGE_SECTION_HEADER`, in the header block.
            let virtual_size = usize::try_from(unsafe {
                read_u32(base, header + crate::pe::PE_SECTION_VIRTUAL_SIZE_OFF)
            })
            .ok()?;
            let virtual_address = usize::try_from(unsafe {
                read_u32(base, header + crate::pe::PE_SECTION_VIRTUAL_ADDRESS_OFF)
            })
            .ok()?;
            let raw_size = usize::try_from(unsafe {
                read_u32(base, header + crate::pe::PE_SECTION_RAW_SIZE_OFF)
            })
            .ok()?;
            let length = if virtual_size == 0 {
                raw_size
            } else {
                virtual_size.min(raw_size)
            };
            if length == 0 {
                continue;
            }
            let end = virtual_address.checked_add(length)?;
            // SAFETY: a loaded section is mapped at `base + VirtualAddress` for at least its
            // virtual size; the range below is the intersection with the file-backed bytes.
            let section =
                unsafe { core::slice::from_raw_parts(base.add(virtual_address), length) };
            bytes.get_mut(virtual_address..end)?.copy_from_slice(section);
        }
        Some(bytes)
    }
}

#[cfg(not(windows))]
impl Platform for Live {
    /// On Linux there are no Windows modules to find: the ported pins are the Windows build's
    /// and the Linux pins are a later work package (the generated `pins::linux` module is
    /// empty). Verification therefore stays `not_loaded`, never a skipped pass.
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

/// The throttle's period in the C is `& 31`, i.e. one tick in 32; named for the tests and docs.
pub const POLL_PERIOD: u32 = 32;

/// Blocks until no verifier runs, or the deadline passes. Returns whether it went idle.
///
/// For host tests and diagnostics; the game's one production waiter is
/// [`crate::plugin::unload`], through [`Modules::wait_verifier`].
#[must_use]
pub fn wait_idle<P: Platform + 'static>(modules: &Modules<P>, deadline: Duration) -> bool {
    modules.wait_verifier(deadline)
}

#[cfg(test)]
mod tests {
    use super::{need_bit, FileHash, LoadedModule, Modules, Platform, State, MODULE_COUNT, ORDER};
    use crate::generated::pins::PIN_SERVER;

    /// A platform whose mapped images the case gives: the refusal cases never get past the
    /// selection, so reading/hashing/pinning an image is a bug and panics.
    struct Refusing(Vec<LoadedModule>);

    impl Platform for Refusing {
        fn module_base(&self, name: &str) -> Option<u64> {
            // The cheap by-name question `poll` asks first: answer whatever is mapped.
            if name.eq_ignore_ascii_case("server.dll") {
                self.0.first().map(|image| image.base)
            } else {
                None
            }
        }

        fn loaded(&self, name: &str) -> Vec<LoadedModule> {
            if name.eq_ignore_ascii_case("server.dll") {
                self.0.clone()
            } else {
                Vec::new()
            }
        }

        fn host_path(&self) -> Option<String> {
            Some(r"Z:\cs2\game\bin\win64\cs2.exe".to_string())
        }

        fn image(&self, _name: &str, _base: u64) -> Option<Vec<u8>> {
            panic!("a refused image is never read");
        }

        fn file_hash(&self, _name: &str, _base: u64) -> FileHash {
            panic!("a refused image is never hashed");
        }

        fn pin(&self, _name: &str, _base: u64, _rva: u32) -> bool {
            panic!("a refused image is never pinned");
        }
    }

    /// Walk twice over `images` and answer (`state`, `base`, log): the second walk proves the
    /// refusal line is written once per module per process.
    fn refusal_case(name: &str, images: Vec<LoadedModule>) -> (Option<State>, u64, String) {
        let _guard = crate::hooks::test_guard::lock();
        let scratch = crate::hooks::test_guard::scratch_log(name);
        let modules = Modules::new(Refusing(images));
        modules.verify_all();
        modules.verify_all();
        (
            modules.state(PIN_SERVER),
            modules.base(PIN_SERVER),
            scratch.text(),
        )
    }

    /// Only Metamod's loader stub mapped: the walk leaves the module `not_loaded`, never reads
    /// the stub, and writes the refusal line once (the module is asked again on every walk).
    #[test]
    fn a_stub_only_module_stays_not_loaded_and_says_so_once() {
        // The real Metamod layout (review N3): the stub lives under the game root, in
        // `csgo\addons\metamod`.
        let images = vec![LoadedModule {
            base: 0x1000_0000,
            path: r"Z:\cs2\game\csgo\addons\metamod\bin\win64\server.dll".to_string(),
        }];
        let (state, base, text) = refusal_case("modules-stub-only", images);
        assert_eq!(state, Some(State::NotLoaded));
        assert_eq!(base, 0, "never based on the stub");
        assert_eq!(
            text.matches("verify: server.dll: the only image mapped under this name is Metamod's loader stub; not used").count(),
            1,
            "one refusal line per module per process: {text}"
        );
    }

    /// Two images under the game root: a refused name stays `not_loaded` (never verified against
    /// a guess) and writes its one line, once.
    #[test]
    fn two_game_images_are_refused_and_the_module_stays_not_loaded() {
        let images = vec![
            LoadedModule {
                base: 0x2000_0000,
                path: r"Z:\cs2\game\csgo\bin\win64\server.dll".to_string(),
            },
            LoadedModule {
                base: 0x3000_0000,
                path: r"Z:\cs2\game\csgo\bin\win64\sub\server.dll".to_string(),
            },
        ];
        let (state, base, text) = refusal_case("modules-ambiguous", images);
        assert_eq!(state, Some(State::NotLoaded));
        assert_eq!(base, 0, "never based on a guess");
        assert_eq!(
            text.matches(
                "verify: server.dll: more than one game image is mapped under this name; not used"
            )
            .count(),
            1,
            "one refusal line per module per process: {text}"
        );
    }

    /// Every pin exactly once - the C's comment at `bridge/mcp_bridge.c:1375-1377` says a
    /// duplicate means a module is never verified.
    #[test]
    fn order_is_a_permutation() {
        let mut seen = [false; MODULE_COUNT];
        for module in ORDER {
            let index = usize::from(module);
            assert!(
                !seen[index],
                "module {module} is in the verifier order twice"
            );
            seen[index] = true;
        }
        assert!(seen.iter().all(|seen| *seen), "the order omits a module");
    }

    /// `need_bit` is a one-bit mask per module and all twelve differ.
    #[test]
    fn need_bits_are_distinct_single_bits() {
        let mut mask = 0;
        for module in 0..MODULE_COUNT as u8 {
            let bit = need_bit(module);
            assert_eq!(bit.count_ones(), 1, "module {module} bit {bit:#x}");
            assert_eq!(mask & bit, 0, "module {module} repeats a bit");
            mask |= bit;
        }
        assert_eq!(mask, 0xfff);
        assert_eq!(need_bit(255), 0);
    }
}
