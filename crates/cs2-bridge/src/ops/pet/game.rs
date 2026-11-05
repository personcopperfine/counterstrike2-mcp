//! The game half of `pet.*`: pet.dll's objects and pinned entry points, the host file facts, and
//! the thin readers that turn them into the plain inputs [`super::reply`] builds from.
//!
//! The C's `pet.*` code is one block of `mcp_bridge_pet.inc` that mixes both halves. It is split
//! here so the wire bytes are testable without a game:
//!
//! * the `op_*` functions in this file are the C's method bodies with every game access replaced
//!   by a [`platform`] call. They run on both targets; the parameter checks (the C's
//!   `InvalidParams` arms) and the reply builders are shared, so `tests/ops_pet_params.rs` drives
//!   them through the real handlers on the host.
//! * [`platform`] is the Windows half - the C's object walks and calls. On a non-Windows target
//!   the same functions exist but answer the loud [`unsupported`] error: pet.dll is a Windows
//!   DLL and this crate's Linux build has no equivalent. That path is unreachable in production
//!   (the `-32017` gate refuses every `pet.*` row before a body runs, because `PIN_PET` and
//!   `PIN_TF2` never verify off Windows), and it is a Rust-only answer, noted in
//!   `bridge/protocol-diff-m3-pet.md`.
//!
//! # The main thread
//!
//! Every `op_*` body runs on the game's main thread (the tick's drain), as in the C. The
//! process-wide state the C keeps in statics - the resolved `CPetApp`, the `pet.load` latch, the
//! KV3 context - lives in [`platform`] and is touched only there.

use crate::kv3_check;
use crate::log;
use crate::ops::OpErr;

use super::reply::{self, Buf, TypeArray};

/// The C's `PetLiveDoc` (`mcp_bridge_pet.inc:814-820`): the five objects that the live-edit
/// paths hand from call to call. Opaque pointers, because the game's types are Windows-only.
#[derive(Clone, Copy)]
pub struct LiveDoc {
    /// `CPetSession`.
    pub session: *mut core::ffi::c_void,
    /// `CPetDoc`.
    pub doc: *mut core::ffi::c_void,
    /// `CPetRootNode` (`doc->m_pRoot`).
    pub root: *mut core::ffi::c_void,
    /// `doc->m_pUndoHistory`.
    pub hist: *mut core::ffi::c_void,
    /// `hist->m_pPartner` - the redo history.
    pub partner: *mut core::ffi::c_void,
}

/// The flags and arrays `pet_live_tail` reports (`mcp_bridge_pet.inc:877-893`).
pub struct LiveTail {
    /// `IsModified(session)`.
    pub dirty: bool,
    /// `hist->HasEntries()`.
    pub can_undo: bool,
    /// `partner->HasEntries()`.
    pub can_redo: bool,
    /// The root's nine arrays.
    pub arrays: [TypeArray; reply::ARRAY_TYPES],
}

/// What `active_pet` + `pet_doc_json` saw for the active document.
///
/// One value per call, built on the main thread and never collected, so the document variant's
/// size is not worth a box (the C's `pet_doc_json` writes straight into the reply buffer).
#[allow(clippy::large_enum_variant)]
pub enum ActiveDoc {
    /// No usable session; the C's short diagnostic (`mcp_bridge_pet.inc:153-169`).
    NoSession(&'static str),
    /// A session, with the document state.
    Doc(reply::DocJson),
}

/// `GetPath`'s answer, with `md_str`'s cut flag (`mcp_bridge.c:5169-5186`): `whole` is false
/// when the path did not fit the C's 1024-byte buffer, and the text is then the prefix.
///
/// `text` is the game's own bytes, not a decoded string: the same bytes are handed back to
/// `Save` and opened by the host gate, so a code-page path must not be re-encoded (the C passes
/// the `char *` straight through).
pub struct PathText {
    /// The buffer's contents.
    pub text: Vec<u8>,
    /// Whether the whole string fit (`md_str` returned 1).
    pub whole: bool,
}

/// `pet_read_vpcf`'s answer (`mcp_bridge_pet.inc:263-299`).
pub enum HostRead {
    /// The C's `why`: `"too_large"` or `"read_failed"`.
    Refused(&'static str),
    /// The file's bytes.
    Bytes(Vec<u8>),
}

// ---------------------------------------------------------------------------------------------
// The method bodies: the C's `op_pet_*` with the game access behind `platform`
// ---------------------------------------------------------------------------------------------

/// `op_pet_load` (`mcp_bridge_pet.inc:421-458`): the framework call and the one-shot latch are
/// platform state, so the body is the platform's.
pub fn op_load(out: &mut Buf) -> Result<(), OpErr> {
    platform::load(out)
}

/// `op_pet_active`'s document shape (`mcp_bridge_pet.inc:1133-1153`).
pub fn op_active(out: &mut Buf) -> Result<(), OpErr> {
    let active = platform::active_doc();
    let read = match &active {
        ActiveDoc::NoSession(reason) => reply::ActiveRead::None(reason),
        ActiveDoc::Doc(doc) => reply::ActiveRead::Doc(doc),
    };
    reply::active_json(out, &read)
}

/// `op_pet_open` (`mcp_bridge_pet.inc:466-535`).
pub fn op_open(params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    // Every path-shape refusal precedes the gate (`:467-509`), and the gate precedes OpenFile.
    let path = reply::open_params(params)?;
    if let Some(refused) = open_gate(&path) {
        log::line(&refused.log); // :402 / :409
        return Err(refused.error);
    }
    let Some(app) = platform::resolve_pet() else {
        return Err(OpErr::new(
            -32003,
            "NoActiveDocument",
            "pet_unavailable",
        )); // :513
    };
    let opened = platform::open_file(app, &path); // :514: flags 1, the session or null
    log::line("pet: open "); // :515-516
    log::line(&path);
    log::line(&format!("pet: open rc {}", opened as usize)); // :517 (`lognum`, the pointer)
    let opened = !opened.is_null();
    let active = platform::active_doc();
    let active = match &active {
        // :518-519: the opened session when it succeeded, else the prior one.
        ActiveDoc::NoSession(_) => None,
        ActiveDoc::Doc(doc) => Some(doc),
    };
    reply::open_json(out, opened, path.as_bytes(), active)
}

/// `op_pet_reload` (`mcp_bridge_pet.inc:1155-1212`).
pub fn op_reload(params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    let session = platform::active_session()
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :1156-1158
    let dirty = platform::is_modified(session);
    let force = reply::force_flag(params);
    if dirty && !force {
        return Err(OpErr::new(
            -32016,
            "UnsavedChanges",
            "document is dirty; pass force:true to discard",
        )); // :1165-1168
    }
    // :1169-1170: captured before the reload.
    let path = platform::get_path(session);
    if platform::is_new(session) || path.text.is_empty() {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no path to reload from",
        )); // :1172-1176
    }
    // :1177-1199: a file pet cannot parse must never reach ReloadDocument.
    if !path.whole {
        let refused = reply::Refused::new("reload", "read_failed", &path.text);
        log::line(&refused.log);
        return Err(refused.error);
    }
    let bytes = match platform::read_vpcf(&path.text) {
        HostRead::Refused(why) => {
            let refused = reply::Refused::new("reload", why, &path.text);
            log::line(&refused.log);
            return Err(refused.error); // :1186-1191
        }
        HostRead::Bytes(bytes) => bytes,
    };
    if let Err(refusal) = crate::kv3_check::vpcf_check(&bytes) {
        let refused = reply::Refused::new("reload", refusal.reason.as_str(), &path.text);
        log::line(&refused.log);
        return Err(refused.error); // :1192-1199
    }
    platform::reload_document(session); // :1200: replaces the CPetDoc
    let (reloaded, has_root) = platform::document_presence(); // :1201-1204
    if !reloaded {
        log::line("pet: reload left no valid document"); // :1204
    }
    log::line(&format!("pet: reload, dirty was {}", u8::from(dirty))); // :1205
    reply::reload_json(out, reloaded, has_root, &path.text, dirty);
    Ok(())
}

/// `op_pet_save` (`mcp_bridge_pet.inc:1213-1245`).
pub fn op_save(params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    let session = platform::active_session()
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :1214-1216
    if platform::is_new(session) {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no path (Save As would open a modal)",
        )); // :1217-1220
    }
    // :1221-1223: copied before the save (Save may reallocate the path string).
    let path = platform::get_path(session);
    if !path.whole || path.text.is_empty() {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no usable path (Save As would open a modal)",
        )); // :1223-1226
    }
    let dirty = platform::is_modified(session); // :1227
    let force = reply::force_flag(params); // :1228-1233
    let mut saved = true; // :1234
    let mut skipped = true;
    if dirty || force {
        skipped = false;
        saved = platform::save_document(session, &path.text); // :1237 the FileSave call
        if saved {
            platform::set_unmodified(session); // :1238 mirrors FileSave
        }
    }
    reply::save_json(out, saved, skipped, &path.text);
    Ok(())
}

/// `op_pet_function_types` (`mcp_bridge_pet.inc:589-633`): `(void)params`.
pub fn op_function_types(_params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    platform::function_types(out)
}

/// `op_pet_schema` (`mcp_bridge_pet.inc:723-783`).
pub fn op_schema(params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    let class = reply::schema_params(params)?; // :724-737
    let read = platform::schema_read(&class)?; // :738-758
    reply::schema_json(out, &class, &read)
}

/// `op_pet_text` (`mcp_bridge_pet.inc:898-954`).
pub fn op_text(_params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    let live = platform::live_doc()?; // :901
    let Some(text_path) = reply::text_path(&platform::local_app_data()) else {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "text_path_too_long",
        )); // :903-906
    };
    if let Err(error) = platform::save_to_file(&live, &text_path) {
        // :908-918
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            &reply::save_failure_message(error.as_deref()),
        ));
    }
    let text = match platform::read_vpcf(text_path.as_bytes()) {
        HostRead::Refused(why) => return Err(reply::text_read_error(why)), // :923-927
        HostRead::Bytes(bytes) => bytes,
    };
    if text.len() > reply::PET_TEXT_MAX {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "text_too_large",
        )); // :928-932
    }
    if text.contains(&0) {
        // :934-938: `text` would be cut at the NUL while `bytes` is not.
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "text_has_nul",
        ));
    }
    // :945-952: the reported path is the document's own GetPath, not the private file.
    let doc_path = platform::get_path(live.session);
    let dirty = platform::is_modified(live.session);
    let is_new = platform::is_new(live.session);
    let bytes = text.len() as u64;
    reply::text_json(
        out,
        &reply::TextReply {
            text: &text,
            bytes,
            path: &doc_path.text,
            dirty,
            is_new,
            can_undo: platform::has_entries(live.hist),
            can_redo: platform::has_entries(live.partner),
        },
    )
}

/// `op_pet_apply` (`mcp_bridge_pet.inc:958-1094`): unescape -> vpcf_check -> prologue ->
/// LoadKV3 -> ONE undo step that replaces the live tree. A failure never reaches BeginUndoStep.
pub fn op_apply(params: Option<&str>, out: &mut Buf) -> Result<(), OpErr> {
    let parsed = match reply::apply_params(params) {
        Ok(parsed) => parsed,
        Err(reply::ApplyRefusal::Invalid(error)) => return Err(error),
        Err(reply::ApplyRefusal::Unloadable { error, log: line }) => {
            log::line(&line); // :1017
            return Err(error);
        }
    };
    let live = platform::live_doc()?; // :1022-1023
    platform::apply_text(&live, &parsed.text, &parsed.label)?; // :1024-1087
    let tail = platform::live_tail(&live); // :1091
    reply::apply_json(
        out,
        &parsed.label,
        parsed.text.len() as u64,
        tail.dirty,
        tail.can_undo,
        tail.can_redo,
        &tail.arrays,
    );
    Ok(())
}

/// `op_pet_undo_redo` (`mcp_bridge_pet.inc:1099-1119`): an empty history is a result.
pub fn op_undo_redo(out: &mut Buf, redo: bool) -> Result<(), OpErr> {
    let live = platform::live_doc()?; // :1101-1102
    let target = if redo { live.partner } else { live.hist }; // :1103
    let has = platform::has_entries(target); // :1104
    if has {
        platform::undo_redo(&live, redo); // :1110-1114: apply-and-pop + the two notifications
    }
    let tail = platform::live_tail(&live); // :1116: read AFTER the tree changed
    reply::undo_redo_json(
        out,
        has,
        if has {
            None
        } else {
            Some(if redo {
                "nothing_to_redo"
            } else {
                "nothing_to_undo"
            })
        },
        tail.dirty,
        tail.can_undo,
        tail.can_redo,
        &tail.arrays,
    ); // :1106-1117
    Ok(())
}

/// The Rust-only answer of a `pet.*` body on a target with no pet.dll half. Unreachable in
/// production: the `-32017` gate refuses every row of this group before a body runs.
#[must_use]
pub fn unsupported() -> OpErr {
    OpErr::new(
        -32603,
        "InternalError",
        "pet.* needs the Windows target: this build has no pet.dll half",
    )
}

// ---------------------------------------------------------------------------------------------
// The Windows half
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
pub(crate) mod platform {
    use core::cell::UnsafeCell;
    use core::ffi::{c_char, c_int, c_void};
    use core::ptr::{addr_of, addr_of_mut};
    use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
    use std::sync::OnceLock;

    use s2sdk::module::Module;
    use s2sdk_sys::generated::{
        CBaseEditorApp, CBaseEditorSession, CBasePetNode, CBasePetNode_slots, CPetDoc,
        CPetSession, CPetSession_slots, CSchemaClassInfo, CSchemaType, CUndoHistory,
        CUndoHistory_slots, CUtlVector_CEntityHandle,
        IParticleFunctionDefinition, IParticleFunctionDefinition_slots, IParticleSystemMgr,
        IParticleSystemMgr_slots, ISchemaSystem, ISchemaSystemTypeScope,
        ISchemaSystemTypeScope_slots, ISchemaSystem_slots, IToolFramework2, IToolFramework2_slots,
        SchemaBaseClassInfoData_t, SchemaClassFieldData_t, SchemaClassInfoData_t,
        SchemaMetadataEntryData_t, SCHEMASYSTEM_INTERFACE_VERSION, TOOLFRAMEWORK2_INTERFACE_VERSION,
    };

    use crate::generated::pins::{self, Pin};
    use crate::log;
    use crate::modules;
    use crate::ops::ident::{mem_ok, mod_iface, obj_is};
    use crate::ops::OpErr;

    use super::super::reply::{self, Buf, ClassSchema, SchemaField, SchemaRead, TypeArray, TypeRow};
    use super::{ActiveDoc, HostRead, LiveDoc, LiveTail, PathText};

    /// `S2_TOOLSYSTEM2_VERSION` (`bridge/s2sdk.h:55`): what pet.dll's `CreateInterface` answers.
    /// A literal, because upstream has no macro for it.
    const TOOLSYSTEM2_VERSION: &str = "ToolSystem2_001";

    /// `S2_SYM_CREATEINTERFACE` (`bridge/s2sdk.h:59`): upstream `tier0/interface.h`'s factory
    /// export name.
    const CREATEINTERFACE_SYM: &str = "CreateInterface";

    /// `CPetSession`'s `GetPath` buffer (`mcp_bridge_pet.inc:221`, `:1169`, `:1221`).
    const PATH_CAP: usize = 1024;

    /// The bytes left in the C's `char msg[192]` once `save_to_file_failed` and `": "` are in it
    /// (`mcp_bridge_pet.inc:909-915`): `192 - slen("save_to_file_failed: ")`.
    const SAVE_ERROR_CAP: usize = 192 - 21;

    /// The class-name buffer (`pet_node_class`, `mcp_bridge_pet.inc:200`, `:541`, `:552`).
    const CLASS_CAP: usize = 128;

    /// The C's `md_str` answer (`mcp_bridge.c:5169-5186`): 1 the whole string fit, -1 cut at
    /// `cap - 1`, 0 null or unreadable (`out[0] = 0`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum StrRead {
        Whole,
        Cut,
        Unreadable,
    }

    // -----------------------------------------------------------------------------------------
    // Module facts
    // -----------------------------------------------------------------------------------------

    /// The C's `mod_ok` (`mcp_bridge.c:1156`): verified and pinned, hence addressed.
    fn mod_ok(module: u8) -> bool {
        modules::live().is_verified(module)
    }

    /// The pin's RVA inside its module, as an absolute address. Only meaningful once the module
    /// gate passed - every caller here is behind `mod_ok` / `obj_is`.
    fn pin_addr(pin: Pin) -> usize {
        usize::try_from(modules::live().base(pin.module)).expect("a module base fits a usize")
            + usize::try_from(pin.value).expect("a pin RVA fits a usize")
    }

    /// A vtable RVA as [`obj_is`] takes it. Pinned RVAs are 32-bit by construction.
    fn vt_rva(pin: Pin) -> u32 {
        u32::try_from(pin.value).expect("a pinned vtable RVA fits 32 bits")
    }

    /// `u8 *pb = (u8 *)g_modBase[PIN_PET]` (`mcp_bridge_pet.inc:107`).
    fn pet_base() -> usize {
        usize::try_from(modules::live().base(pins::PIN_PET)).expect("a module base fits a usize")
    }

    /// `mod_state_name(PIN_PET)` (`mcp_bridge_pet.inc:429`, `:455`).
    fn module_state_name() -> &'static str {
        modules::live().state_name(pins::PIN_PET)
    }

    /// `GetModuleHandleA(k_pinModules[PIN_PET].name)` (`mcp_bridge_pet.inc:427`, `:445`), asked
    /// of the one game-module finder: the game's own `pet.dll`, not any other image of the name
    /// (and never a guess when the name is ambiguous).
    fn pet_mapped() -> bool {
        let name = pins::PIN_MODULES[usize::from(pins::PIN_PET)].name;
        Module::find_game(name).ok().flatten().is_some()
    }

    /// The C's `pet_vfn` (`mcp_bridge_pet.inc:39-42`): entry `slot` of the vtable `obj` points
    /// at, as a typed function pointer.
    ///
    /// # Safety
    ///
    /// `obj` must be an object whose identity check (`obj_is` with the class's pinned vtable)
    /// passed, so its vptr is the pinned vtable; `slot` must be one of that class's generated
    /// slot constants, which are inside its `VTLEN`. `T` must be the slot's own signature.
    unsafe fn pet_vfn<T: Copy>(obj: *const c_void, slot: usize) -> T {
        // SAFETY: the vptr lies at offset 0 of every polymorphic object (the C reads it the
        // same way), and the caller's identity check pinned it to the class's vtable.
        let vptr = unsafe { obj.cast::<*const *const c_void>().read_unaligned() };
        // `void **vt = *(void ***)obj; return vt[slot];` (`:40-41`): the entries are pointer
        // sized, so the index scales by 8 and not by `c_void`'s one byte.
        let entries = vptr.cast::<*const c_void>();
        // SAFETY: `slot` is a generated slot constant of that class, inside its generated VTLEN.
        let entry = unsafe { *entries.add(slot) };
        // SAFETY: `T` is the signature of the vtable entry at `slot`, per the generated
        // declaration; the bit pattern is a function pointer of that type.
        unsafe { core::mem::transmute_copy(&entry) }
    }

    // -----------------------------------------------------------------------------------------
    // md_str
    // -----------------------------------------------------------------------------------------

    /// `md_str` (`mcp_bridge.c:5169-5186`): copy a game-owned C string, probing readability
    /// first and again at every page boundary (strings can point at freed or reallocated
    /// `CUtlString` storage). `out` is the C's `char *out, u32 cap`.
    fn md_str(s: *const c_char, out: &mut [u8]) -> StrRead {
        if out.is_empty() {
            return StrRead::Unreadable;
        }
        out[0] = 0;
        if s.is_null() || !mem_ok(s.cast()) {
            return StrRead::Unreadable;
        }
        let cap = out.len();
        let mut i = 0usize;
        while i + 1 < cap {
            // Every page boundary is re-probed (`mcp_bridge.c:5178`).
            if i != 0 && (s as usize).wrapping_add(i) & 0xFFF == 0 && !mem_ok(s.wrapping_add(i).cast())
            {
                out[0] = 0;
                return StrRead::Unreadable;
            }
            // SAFETY: `mem_ok` proved the first byte and every page boundary so far; the C reads
            // `s[i]` unguarded between probes (`:5180`).
            let byte = unsafe { s.add(i).read() } as u8;
            out[i] = byte;
            if byte == 0 {
                return StrRead::Whole;
            }
            i += 1;
        }
        out[i] = 0;
        StrRead::Cut
    }

    /// [`md_str`] into a fresh buffer, cut at `cap - 1`.
    fn read_str(s: *const c_char, cap: usize) -> (StrRead, Vec<u8>) {
        let mut out = vec![0u8; cap];
        let read = md_str(s, &mut out);
        (read, reply::cstr(&out).to_vec())
    }

    /// [`md_str`]'s text as a `String`, `""` when unreadable (the C's `out[0] = 0`).
    fn read_text(s: *const c_char, cap: usize) -> String {
        let (_, bytes) = read_str(s, cap);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // -----------------------------------------------------------------------------------------
    // pet_bind: the pinned, non-virtual entry points (`mcp_bridge_pet.inc:105-126`)
    // -----------------------------------------------------------------------------------------

    type RootChildCountFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type RootChildFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> *mut c_void;
    type DocBeginUndoStepFn = unsafe extern "C" fn(*mut c_void, *const c_char, u8);
    type DocUndoFn = unsafe extern "C" fn(*mut c_void);
    type DocSaveToFileFn =
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut *const c_char, *mut c_void) -> u8;
    type RootLoadFromKv3Fn = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type RootRefreshCachesFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type Kv3CtxFn = unsafe extern "C" fn(*mut c_void, c_int);
    type Kv3CtxRootFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type Kv3CtxDtorFn = unsafe extern "C" fn(*mut c_void);
    type SessionOnDocModifiedFn = unsafe extern "C" fn(*mut c_void);
    type NotifyFuncListFn = unsafe extern "C" fn(*mut u64);
    type SchemaTypeScopeFn = unsafe extern "C" fn() -> *mut c_void;
    type Kv3StripToSchemaFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type RootBehaviorVersionFn = unsafe extern "C" fn(*mut c_void) -> u32;
    type RootFilterFunctionsFn = unsafe extern "C" fn(*mut c_void, u32);
    type LoadKv3Fn = unsafe extern "C" fn(
        *mut c_void,
        *mut *const c_char,
        *const c_char,
        *const c_void,
        *const c_char,
        u32,
    ) -> u8;

    /// The C's `g_petChild*` / `g_pet*` function table, bound from the pinned RVAs.
    #[derive(Clone, Copy)]
    struct PetFns {
        child_count: RootChildCountFn,
        child: RootChildFn,
        begin_undo_step: DocBeginUndoStepFn,
        undo: DocUndoFn,
        redo: DocUndoFn,
        save_to_file: DocSaveToFileFn,
        root_load_from_kv3: RootLoadFromKv3Fn,
        refresh_caches: RootRefreshCachesFn,
        kv3_ctx_ctor: Kv3CtxFn,
        kv3_ctx_root: Kv3CtxRootFn,
        kv3_ctx_dtor: Kv3CtxDtorFn,
        session_on_doc_modified: SessionOnDocModifiedFn,
        notify_func_list: NotifyFuncListFn,
        type_scope: SchemaTypeScopeFn,
        strip_to_schema: Kv3StripToSchemaFn,
        behavior_version: RootBehaviorVersionFn,
        filter_functions: RootFilterFunctionsFn,
    }

    /// `pet_bind` (`mcp_bridge_pet.inc:105-126`): the entry points from their pinned RVAs,
    /// exactly as the C's `pb + PETFN_*`. The C latches on `g_petBound`; the addresses only
    /// depend on the pinned module base, so a [`OnceLock`] is the same thing.
    ///
    /// Every call site has already passed the module gate (`resolve_pet` / `active_pet` /
    /// `pet_doc_json`'s `obj_is`), which is what makes the RVAs meaningful - the C's
    /// `if (g_petBound || !mod_ok(PIN_PET)) return;` guard, restated as this precondition.
    fn pet_fns() -> PetFns {
        static FNS: OnceLock<PetFns> = OnceLock::new();
        *FNS.get_or_init(|| {
            // SAFETY: a pinned function RVA inside the verified pet.dll image has the declared
            // signature (the sigs entry byte-checks its prologue); the same casts the C does at
            // `mcp_bridge_pet.inc:108-124`.
            unsafe {
                PetFns {
                    child_count: core::mem::transmute::<usize, RootChildCountFn>(pin_addr(
                        pins::PETFN_ROOT_GETCHILDCOUNT,
                    )),
                    child: core::mem::transmute::<usize, RootChildFn>(pin_addr(
                        pins::PETFN_ROOT_GETCHILD,
                    )),
                    begin_undo_step: core::mem::transmute::<usize, DocBeginUndoStepFn>(pin_addr(
                        pins::PETFN_DOC_BEGINUNDOSTEP,
                    )),
                    undo: core::mem::transmute::<usize, DocUndoFn>(pin_addr(pins::PETFN_DOC_UNDO)),
                    redo: core::mem::transmute::<usize, DocUndoFn>(pin_addr(pins::PETFN_DOC_REDO)),
                    save_to_file: core::mem::transmute::<usize, DocSaveToFileFn>(pin_addr(
                        pins::PETFN_DOC_SAVETOFILE,
                    )),
                    root_load_from_kv3: core::mem::transmute::<usize, RootLoadFromKv3Fn>(
                        pin_addr(pins::PETFN_ROOT_LOADFROMKV3),
                    ),
                    refresh_caches: core::mem::transmute::<usize, RootRefreshCachesFn>(
                        pin_addr(pins::PETFN_ROOT_REFRESHCACHES),
                    ),
                    kv3_ctx_ctor: core::mem::transmute::<usize, Kv3CtxFn>(pin_addr(
                        pins::PETFN_KV3CTX_CTOR,
                    )),
                    kv3_ctx_root: core::mem::transmute::<usize, Kv3CtxRootFn>(pin_addr(
                        pins::PETFN_KV3CTX_ROOT,
                    )),
                    kv3_ctx_dtor: core::mem::transmute::<usize, Kv3CtxDtorFn>(pin_addr(
                        pins::PETFN_KV3CTX_DTOR,
                    )),
                    session_on_doc_modified: core::mem::transmute::<usize, SessionOnDocModifiedFn>(
                        pin_addr(pins::PETFN_SESSION_ONDOCMODIFIED),
                    ),
                    notify_func_list: core::mem::transmute::<usize, NotifyFuncListFn>(pin_addr(
                        pins::PETFN_NOTIFY_FUNCLIST,
                    )),
                    type_scope: core::mem::transmute::<usize, SchemaTypeScopeFn>(pin_addr(
                        pins::PETFN_SCHEMA_TYPESCOPE,
                    )),
                    strip_to_schema: core::mem::transmute::<usize, Kv3StripToSchemaFn>(
                        pin_addr(pins::PETFN_KV3_STRIP_TO_SCHEMA),
                    ),
                    behavior_version: core::mem::transmute::<usize, RootBehaviorVersionFn>(
                        pin_addr(pins::PETFN_ROOT_BEHAVIORVERSION),
                    ),
                    filter_functions: core::mem::transmute::<usize, RootFilterFunctionsFn>(
                        pin_addr(pins::PETFN_ROOT_FILTER_FUNCTIONS),
                    ),
                }
            }
        })
    }

    // -----------------------------------------------------------------------------------------
    // resolve_pet / active_pet (`mcp_bridge_pet.inc:130-169`)
    // -----------------------------------------------------------------------------------------

    /// The resolved `CPetApp` (C `g_pet`): resolved once, pinned module -> stable identity.
    static PET_APP: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

    /// `resolve_pet` (`mcp_bridge_pet.inc:130-150`).
    pub fn resolve_pet() -> Option<*mut c_void> {
        let cached = PET_APP.load(Ordering::Acquire);
        if !cached.is_null() {
            return Some(cached);
        }
        if !mod_ok(pins::PIN_PET) {
            log::line("pet: module not verified"); // :132
            return None;
        }
        let base = pet_base();
        let name = pins::PIN_MODULES[usize::from(pins::PIN_PET)].name;
        // The C's `GetModuleHandleA(name)` (`:134-135`), asked of the one game-module finder so
        // the answer is the game's own image, and its base must be the verified one.
        let Some(module) = Module::find_game(name).ok().flatten() else {
            log::line("pet: module handle changed"); // `GetModuleHandleA` answered 0 (`:134-135`)
            return None;
        };
        if module.image().base() != u64::try_from(base).expect("a module base fits u64") {
            log::line("pet: module handle changed"); // :135
            return None;
        }
        let Some(create_interface) = module.symbol(CREATEINTERFACE_SYM) else {
            log::line("pet: CreateInterface export missing"); // :137
            return None;
        };
        // `S2CreateInterfaceFn` (`bridge/s2sdk.h:58`): `void *(const char *, int *)`.
        type CreateInterfaceFn =
            unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
        // SAFETY: the export is tier0's factory over the module's interfaces, declared with this
        // signature in `tier0/interface.h`; both arguments outlive the call.
        let create_interface: CreateInterfaceFn = unsafe { core::mem::transmute(create_interface) };
        let Ok(version) = std::ffi::CString::new(TOOLSYSTEM2_VERSION) else {
            return None;
        };
        let mut ret: c_int = -1;
        // SAFETY: as above; `ret` is the factory's out-parameter and `version` a live C string.
        let app = unsafe { create_interface(version.as_ptr(), &mut ret) };
        if app.is_null() || ret != 0 {
            // ret 0 = found (`:140`).
            // `lognum` prints the sign-extended int as an unsigned decimal (`:208-217`).
            log::line(&format!(
                "pet: ToolSystem2_001 unresolved, ret={}",
                ret as i64 as u64
            )); // :141
            return None;
        }
        if app as usize != base + usize::try_from(pins::PETG_APP_OBJ.value).expect("RVA") {
            log::line("pet: app identity mismatch"); // :144
            return None;
        }
        if !obj_is(app, pins::PIN_PET, vt_rva(pins::PETV_APP_VT)) {
            log::line("pet: app vtable mismatch"); // :145
            return None;
        }
        PET_APP.store(app, Ordering::Release); // :147
        log::line("pet: app resolved (ToolSystem2_001 export, ret 0)"); // :148
        Some(app)
    }

    /// `CPetApp_ActiveSession` (`bridge/s2sdk.h:625`): `app->m_pActiveSession`.
    ///
    /// # Safety
    ///
    /// `app` is the identity-checked `CPetApp`, whose first base is
    /// `CBaseEditorSharedWindowApp` -> `CBaseEditorApp` (`bridge/s2sdk.h:620-621`), so the same
    /// address is a `CBaseEditorApp`.
    unsafe fn app_active_session(app: *mut c_void) -> *mut CPetSession {
        // SAFETY: per the contract above; `m_pActiveSession` may be null (no document open).
        unsafe { (*app.cast::<CBaseEditorApp>()).m_pActiveSession.cast() }
    }

    /// `active_pet` (`mcp_bridge_pet.inc:153-169`): the active session with per-hop validation.
    pub fn active_session() -> Result<*mut c_void, &'static str> {
        if !mod_ok(pins::PIN_PET) {
            let state = modules::live().state(pins::PIN_PET);
            return Err(match state {
                Some(modules::State::Mismatch) => "pet_mismatch",
                Some(modules::State::Verifying) => "pet_verifying",
                _ => "pet_not_loaded",
            }); // :155-160
        }
        let Some(app) = resolve_pet() else {
            return Err("pet_unavailable"); // :162
        };
        // SAFETY: `app` is the identity-checked singleton.
        let session = unsafe { app_active_session(app) };
        if session.is_null() {
            return Err("no_session"); // :164
        }
        if !obj_is(
            session.cast(),
            pins::PIN_PET,
            vt_rva(pins::PETV_SESSION_VT),
        ) {
            return Err("bad_session"); // :165
        }
        // SAFETY: the identity check above proved `session` is a live `CPetSession`.
        let doc = unsafe { (*session).m_pDoc };
        if doc.is_null() {
            return Err("no_document"); // :166
        }
        if !obj_is(doc.cast(), pins::PIN_PET, vt_rva(pins::PETV_DOC_VT)) {
            return Err("bad_doc"); // :167
        }
        Ok(session.cast())
    }

    // -----------------------------------------------------------------------------------------
    // Session / document calls
    // -----------------------------------------------------------------------------------------

    /// `session->vt->IsModified(session)` (`mcp_bridge_pet.inc:223`). The C's slot returns a
    /// one-byte flag (`bridge/s2sdk.h:627`), read as `u8` - never a Rust `bool` from game code.
    pub fn is_modified(session: *mut c_void) -> bool {
        type IsModifiedFn = unsafe extern "C" fn(*mut CBaseEditorSession) -> u8;
        // SAFETY: `session` passed `obj_is` against the pinned CPetSession vtable, whose
        // `IsModified` slot is `S2_SLOT_CBaseEditorSession_IsModified`
        // (`bridge/s2sdk_gen.h:2176`); the generated constant is the same number.
        unsafe {
            let f: IsModifiedFn = pet_vfn(session, CPetSession_slots::IsModified);
            f(session.cast()) != 0
        }
    }

    /// `session->m_bIsNew` (`:227`, `:1172`, `:1217`).
    pub fn is_new(session: *mut c_void) -> bool {
        // SAFETY: `session` is a live `CPetSession` (identity-checked); `m_bIsNew` is a one-byte
        // MSVC `bool` (`bridge/s2sdk_gen.h:3699` spells it `u8`), read as a byte so a value other
        // than 0/1 cannot be an invalid Rust `bool`.
        let raw = unsafe {
            addr_of!((*session.cast::<CPetSession>()).base.base.base.m_bIsNew)
                .cast::<u8>()
                .read_unaligned()
        };
        raw != 0
    }

    /// `session->vt->GetPath(session)` (`:222`, `:1170`, `:1223`, `:945`).
    pub fn get_path(session: *mut c_void) -> PathText {
        type GetPathFn = unsafe extern "C" fn(*mut CBaseEditorSession) -> *const c_char;
        // SAFETY: as `is_modified`; the slot is `S2_SLOT_CBaseEditorSession_GetPath`
        // (`bridge/s2sdk_gen.h:2178`). The callee returns a game-owned C string, read through
        // `md_str`'s probes and kept as raw bytes: the same bytes go back to `Save` and to the
        // host gate's `CreateFileA`, so a code-page path is never decoded and re-encoded.
        unsafe {
            let f: GetPathFn = pet_vfn(session, CPetSession_slots::GetPath);
            let (read, bytes) = read_str(f(session.cast()), PATH_CAP);
            PathText {
                text: bytes,
                whole: read == StrRead::Whole,
            }
        }
    }

    /// `session->vt->ReloadDocument(session)` (`:1200`).
    pub fn reload_document(session: *mut c_void) {
        type ReloadFn = unsafe extern "C" fn(*mut CBaseEditorSession);
        // SAFETY: as `is_modified`; the slot is `S2_SLOT_CBaseEditorSession_ReloadDocument`
        // (`bridge/s2sdk_gen.h:2172`), and the caller has already run the vpcf gate the C runs
        // first (`:1177-1199`).
        unsafe {
            let f: ReloadFn = pet_vfn(session, CPetSession_slots::ReloadDocument);
            f(session.cast());
        }
    }

    /// `session->vt->Save(session, path, path, 0, 0)` (`:1237`): a one-byte success flag.
    pub fn save_document(session: *mut c_void, path: &[u8]) -> bool {
        type SaveFn = unsafe extern "C" fn(
            *mut CBaseEditorSession,
            *const c_char,
            *const c_char,
            u8,
            u8,
        ) -> u8;
        let Ok(path) = std::ffi::CString::new(path) else {
            return false;
        };
        // SAFETY: as `is_modified`; the slot is `S2_SLOT_CBaseEditorSession_Save`
        // (`bridge/s2sdk_gen.h:2183`), the two `u8` arguments are the C's `0, 0`, and `path`
        // outlives the call.
        unsafe {
            let f: SaveFn = pet_vfn(session, CPetSession_slots::Save);
            f(session.cast(), path.as_ptr(), path.as_ptr(), 0, 0) != 0
        }
    }

    /// `session->vt->SetModified(session, 0)` (`:1238`): the C's `u8` argument
    /// (`bridge/s2sdk.h:628`).
    pub fn set_unmodified(session: *mut c_void) {
        type SetModifiedFn = unsafe extern "C" fn(*mut CBaseEditorSession, u8);
        // SAFETY: as `is_modified`; the slot is `S2_SLOT_CBaseEditorSession_SetModified`
        // (`bridge/s2sdk_gen.h:2177`).
        unsafe {
            let f: SetModifiedFn = pet_vfn(session, CPetSession_slots::SetModified);
            f(session.cast(), 0);
        }
    }

    /// `app->vt->OpenFile(app, path, 1)` (`:514`): the session pointer, null on failure.
    pub fn open_file(app: *mut c_void, path: &str) -> *mut c_void {
        let Ok(path) = std::ffi::CString::new(path) else {
            return core::ptr::null_mut();
        };
        // SAFETY: `app` is the identity-checked `CPetApp`, whose vtable is the pinned one; the
        // slot is `S2_SLOT_CBaseEditorApp_OpenFile` (`bridge/s2sdk_gen.h:2193`) = the generated
        // `CBaseEditorApp_slots` name, and the flags value 1 is what the UI passes
        // (`bridge/s2sdk.h:615`).
        unsafe {
            type OpenFileFn =
                unsafe extern "C" fn(*mut CBaseEditorApp, *const c_char, u32) -> *mut c_void;
            let f: OpenFileFn = pet_vfn(
                app,
                s2sdk_sys::generated::CBaseEditorApp_slots::OpenFile,
            );
            f(app.cast(), path.as_ptr(), 1)
        }
    }

    /// `doc->m_pRoot`, `doc->m_pUndoHistory` (`:224`, `:853-864`).
    ///
    /// # Safety
    ///
    /// `session` is a live `CPetSession` whose `m_pDoc` passed `obj_is`.
    unsafe fn live_doc_from_session(session: *mut CPetSession) -> Result<LiveDoc, OpErr> {
        // SAFETY: the caller's contract.
        let doc = unsafe { (*session).m_pDoc };
        // `pet_live_doc` re-checks every hop (`mcp_bridge_pet.inc:852-868`).
        let root = unsafe { (*doc).m_pRoot };
        if root.is_null() {
            return Err(OpErr::new(-32003, "NoActiveDocument", "no_root")); // :854
        }
        if !obj_is(root, pins::PIN_PET, vt_rva(pins::PETV_ROOTNODE_VT)) {
            return Err(OpErr::new(-32003, "NoActiveDocument", "bad_root")); // :855-858
        }
        let hist = unsafe { (*doc).m_pUndoHistory };
        if hist.is_null() || !obj_is(hist.cast(), pins::PIN_PET, vt_rva(pins::PETV_UNDOHIST_VT)) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "bad_undo_history",
            )); // :859-863
        }
        let partner = unsafe { (*hist).m_pPartner };
        if partner.is_null()
            || !obj_is(partner.cast(), pins::PIN_PET, vt_rva(pins::PETV_UNDOHIST_VT))
        {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "bad_redo_history",
            )); // :864-868
        }
        Ok(LiveDoc {
            session: session.cast(),
            doc: doc.cast(),
            root,
            hist: hist.cast(),
            partner: partner.cast(),
        })
    }

    /// `pet_live_doc` (`mcp_bridge_pet.inc:847-871`): the common prologue of the live-edit ops.
    pub fn live_doc() -> Result<LiveDoc, OpErr> {
        let session = active_session()
            .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :850-851
        // SAFETY: `active_session` returned an identity-checked `CPetSession`.
        unsafe { live_doc_from_session(session.cast()) }
    }

    /// `hist->vt->HasEntries(hist)` (`:880-881`, `:1104`): a one-byte flag
    /// (`bridge/s2sdk.h:663`).
    pub fn has_entries(hist: *mut c_void) -> bool {
        type HasEntriesFn = unsafe extern "C" fn(*mut CUndoHistory) -> u8;
        // SAFETY: `hist` / `partner` passed `obj_is` against the pinned `CUndoHistory` vtable;
        // the slot is `S2_SLOT_CUndoHistory_HasEntries` (`bridge/s2sdk_gen.h:2137`).
        unsafe {
            let f: HasEntriesFn = pet_vfn(hist, CUndoHistory_slots::HasEntries);
            f(hist.cast()) != 0
        }
    }

    /// `pet.undo` / `pet.redo` (`mcp_bridge_pet.inc:1110-1114`): apply-and-pop, then the two
    /// notifications pet's own Undo omits (the preview must recompile, the title must say
    /// modified).
    pub fn undo_redo(live: &LiveDoc, redo: bool) {
        let fns = pet_fns();
        // SAFETY: `live` is the identity-checked chain `pet_live_doc` produced, so the pinned
        // entry points take these objects (`mcp_bridge_pet.inc:1110-1114`).
        unsafe {
            if redo {
                (fns.redo)(live.doc);
            } else {
                (fns.undo)(live.doc);
            }
            // The callee reads `[3]` as the session (`:1111-1112`).
            (fns.notify_func_list)(func_list(live.session));
            (fns.session_on_doc_modified)(live.session);
        }
    }

    /// `pet_live_tail`'s inputs (`mcp_bridge_pet.inc:877-893`), read after the tree changed.
    pub fn live_tail(live: &LiveDoc) -> LiveTail {
        LiveTail {
            dirty: is_modified(live.session),
            can_undo: has_entries(live.hist),
            can_redo: has_entries(live.partner),
            arrays: read_arrays(live.root),
        }
    }

    // -----------------------------------------------------------------------------------------
    // The nine arrays and the node class name (`mcp_bridge_pet.inc:176-212`)
    // -----------------------------------------------------------------------------------------

    /// `pet_node_class` (`mcp_bridge_pet.inc:176-182`): the child's `_class` through the node
    /// vtable's `GetClassNameA`.
    ///
    /// The node cannot be identity-checked (the class varies per config), so the vptr, the slot
    /// and the returned string are each probed, as the C does.
    fn node_class(node: *mut c_void) -> Option<String> {
        if node.is_null() || !mem_ok(node.cast()) {
            return None;
        }
        // SAFETY: `mem_ok` proved the first byte readable; the vptr is at offset 0.
        let vptr = unsafe { node.cast::<*const c_void>().read_unaligned() };
        if vptr.is_null() || !mem_ok(vptr.cast()) {
            return None;
        }
        // The slot pointer must be readable before it is called (`:179`).
        // SAFETY: the slot constant is the generated `S2_SLOT_CBasePetNode_GetClassNameA`.
        let slot = CBasePetNode_slots::GetClassName;
        if !mem_ok(unsafe { vptr.cast::<*const c_void>().add(slot) }.cast()) {
            return None;
        }
        type GetClassNameFn = unsafe extern "C" fn(*mut CBasePetNode) -> *const c_char;
        // SAFETY: the vptr and the slot are readable (probed above) and the slot is the node's
        // `GetClassNameA` (`bridge/s2sdk_gen.h:2292` = slot 18).
        let name = unsafe {
            let f: GetClassNameFn = pet_vfn(node, slot);
            f(node.cast())
        };
        let (read, bytes) = read_str(name, CLASS_CAP);
        (read == StrRead::Whole).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `pet_arrays_json`'s reader half (`mcp_bridge_pet.inc:187-212`): the nine arrays, each
    /// child's class name read in order.
    ///
    /// Stops like the C: the first unusable count or unreadable child returns from
    /// `pet_arrays_json` (`:191-194`, `:201-204`), so no later `GetChildCount`/`GetChild` call is
    /// made. The remaining slots are [`TypeArray::Stopped`].
    fn read_arrays(root: *mut c_void) -> [TypeArray; reply::ARRAY_TYPES] {
        let fns = pet_fns();
        let mut arrays: [TypeArray; reply::ARRAY_TYPES] = core::array::from_fn(|_| TypeArray::Stopped);
        for (t, slot) in arrays.iter_mut().enumerate() {
            // SAFETY: `root` passed `obj_is` against the pinned `CPetRootNode` vtable; the two
            // entry points are pet's own root walkers (`bridge/bridge_pins.h:127-128`).
            let count = unsafe { (fns.child_count)(root, t as c_int) };
            if !(0..=4096).contains(&count) {
                // `:191-194`: the C returns before reading another count.
                *slot = TypeArray::CountUnusable(count);
                break;
            }
            let mut classes = Vec::with_capacity(count as usize);
            let mut bad = false;
            for i in 0..count {
                // `GetChild` has NO bounds check: 0 <= i < count (`:199`).
                // SAFETY: as above.
                let child = unsafe { (fns.child)(root, t as c_int, i) };
                let class = node_class(child);
                bad = class.is_none();
                classes.push(class);
                if bad {
                    break;
                }
            }
            *slot = TypeArray::Children { count, classes };
            if bad {
                // `:201-204`: the C aborts the whole object on the first unreadable child, and
                // makes no further `GetChild` call.
                break;
            }
        }
        arrays
    }

    /// `pet_doc_json`'s input (`mcp_bridge_pet.inc:217-246`).
    pub fn active_doc() -> ActiveDoc {
        let session = match active_session() {
            Ok(session) => session,
            Err(reason) => return ActiveDoc::NoSession(reason),
        };
        // SAFETY: `active_session` returned an identity-checked `CPetSession`.
        let doc = unsafe { (*session.cast::<CPetSession>()).m_pDoc };
        if doc.is_null() {
            return ActiveDoc::Doc(reply::DocJson::NoDoc); // :219
        }
        let head = reply::DocHead {
            path: get_path(session).text, // :222
            dirty: is_modified(session),  // :223
            is_new: is_new(session),      // :227
        };
        // SAFETY: `doc` passed `obj_is` in `active_session`.
        let root = unsafe { (*doc).m_pRoot };
        if root.is_null() {
            return ActiveDoc::Doc(reply::DocJson::NoRoot(head)); // :224
        }
        if !obj_is(root, pins::PIN_PET, vt_rva(pins::PETV_ROOTNODE_VT)) {
            return ActiveDoc::Doc(reply::DocJson::BadRoot(head)); // :233-237
        }
        ActiveDoc::Doc(reply::DocJson::Root(head, read_arrays(root)))
    }

    /// `pet.reload`'s post-reload probe (`mcp_bridge_pet.inc:1201-1204`): whether a valid
    /// session is active, and whether its document has a root.
    pub fn document_presence() -> (bool, bool) {
        match active_session() {
            Err(_) => (false, false),
            Ok(session) => {
                // SAFETY: identity-checked session; `m_pDoc` may be null.
                let doc = unsafe { (*session.cast::<CPetSession>()).m_pDoc };
                let has_root = !doc.is_null()
                    // SAFETY: `doc` is the session's live document when non-null (the
                    // `active_session` check requires `m_pDoc` non-null, so it is set).
                    && !unsafe { (*doc).m_pRoot }.is_null();
                (true, has_root)
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // LoadKV3 and the live edit (`mcp_bridge_pet.inc:785-1094`)
    // -----------------------------------------------------------------------------------------

    /// The KV3 context's 16-aligned static buffer (C `g_petKv3Ctx`,
    /// `mcp_bridge_pet.inc:809`). Main thread only, like every `pet.*` body.
    #[repr(align(16))]
    struct Kv3CtxBuf(#[allow(dead_code)] [u8; reply::PET_KV3CTX_BYTES]); // only reached through
                                                                            // the raw pointer the
                                                                            // KV3 ctor takes

    /// C `g_petKv3Ctx`: a `static` because the freestanding build has no `__chkstk` for a >3 KB
    /// stack local, zeroed before every ctor (the ctor does not clear).
    struct Kv3CtxCell(UnsafeCell<Kv3CtxBuf>);

    // SAFETY: the buffer is only touched by a `pet.*` body, and those all run on the game's main
    // thread (the tick's drain), so there is no concurrent access - the C's `static` has the
    // same contract.
    unsafe impl Sync for Kv3CtxCell {}

    static KV3_CTX: Kv3CtxCell = Kv3CtxCell(UnsafeCell::new(Kv3CtxBuf([0; reply::PET_KV3CTX_BYTES])));

    /// C `g_petSaveCb` (`:812`): the 64-byte zero callback block `PETFN_DOC_SAVETOFILE` reads at
    /// `+0x38` and skips when 0. A `static` the C leaves writable, so this one is a cell rather
    /// than an immutable static: handing out `*mut` to read-only memory would be undefined if the
    /// callee ever wrote it. Main thread only, like the rest of this module.
    struct SaveCb(UnsafeCell<[u8; 64]>);

    // SAFETY: only a `pet.*` body touches it, and those run on the game's main thread.
    unsafe impl Sync for SaveCb {}

    static SAVE_CB: SaveCb = SaveCb(UnsafeCell::new([0; 64]));

    /// C `static u64 flist[4]` in `op_pet_apply` (`:1081-1083`) and `op_pet_undo_redo`
    /// (`:1111-1112`): the function-list notification's argument block, whose `[3]` entry the
    /// callee reads as the session. A `static` in the C (not a stack local), touched on the main
    /// thread only, like the rest of this module; every call writes all four entries first.
    struct FuncList(UnsafeCell<[u64; 4]>);

    // SAFETY: only a `pet.*` body touches it, and those run on the game's main thread.
    unsafe impl Sync for FuncList {}

    static FUNC_LIST: FuncList = FuncList(UnsafeCell::new([0; 4]));

    /// The C's `flist` (`:1081-1083`, `:1111-1112`): three zero words then the session.
    fn func_list(session: *mut c_void) -> *mut u64 {
        let flist = FUNC_LIST.0.get();
        // SAFETY: the four-slot array is the static's own storage, and this body is the only
        // writer on the main thread.
        unsafe {
            (*flist) = [0, 0, 0, session as usize as u64];
        }
        flist.cast::<u64>()
    }

    /// `pet_clear_selection` (`mcp_bridge_pet.inc:837-842`): after the live tree is rebuilt the
    /// UI's selection may point past a shrunken array (`GetChild` has no bounds check), so every
    /// entry is set to pet's own "none" `{9, -1}`.
    fn clear_selection(doc: *mut c_void) {
        // The member is the generic `CUtlVector<PetDocSelection_t>` layout; the generated
        // `CUtlVector_CEntityHandle` is that same 24-byte layout with a different element type
        // (`bridge/s2sdk_gen.h` pins the generic shape with its member-size assert).
        //
        // SAFETY: `doc` is the identity-checked `CPetDoc`; `m_Selection` is that layout.
        let selection = unsafe {
            &*addr_of!((*doc.cast::<CPetDoc>()).m_Selection).cast::<CUtlVector_CEntityHandle>()
        };
        let n = selection.m_Size;
        let sel = selection.m_pMemory.cast::<c_int>();
        if n <= 0
            || n > 4096
            || sel.is_null()
            || !mem_ok(sel.cast())
            // `sel + 2 * n - 1` (`:840`): the last int of the {type, index} pairs.
            || !mem_ok(unsafe { sel.add(2 * n as usize - 1) }.cast())
        {
            return;
        }
        for i in 0..n as usize {
            // SAFETY: the whole pair range was probed above.
            unsafe {
                sel.add(2 * i).write_unaligned(9);
                sel.add(2 * i + 1).write_unaligned(-1);
            }
        }
    }

    /// `op_pet_apply`'s game half (`mcp_bridge_pet.inc:1024-1087`): parse into the static KV3
    /// context, then the ONE undo step that replaces the live tree.
    pub fn apply_text(live: &LiveDoc, text: &[u8], label: &str) -> Result<(), OpErr> {
        let fns = pet_fns();
        let ctx = KV3_CTX.0.get().cast::<c_void>();
        // SAFETY: `ctx` is the 16-aligned 16 KB static buffer and the ctor takes exactly that
        // (`bridge/bridge_pins.h:137`); the bytes are zeroed first because the ctor does not
        // clear them (`mcp_bridge_pet.inc:1025-1026`).
        unsafe {
            core::ptr::write_bytes(ctx.cast::<u8>(), 0, reply::PET_KV3CTX_BYTES);
            (fns.kv3_ctx_ctor)(ctx, 0);
        }
        // SAFETY: `ctx` was just constructed.
        let kv = unsafe { (fns.kv3_ctx_root)(ctx) };
        if kv.is_null() {
            // SAFETY: `ctx` is constructed; the dtor releases it.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "kv3_root_null",
            )); // :1028-1032
        }
        // `:1034-1040`: tier0 must be the verified PIN_TIER0 image - the export is resolved from
        // it, not from an arbitrary copy. The one game-module finder names that image (never a
        // guess when the name is ambiguous).
        let Some(tier0) = Module::find_game(pins::PIN_MODULES[usize::from(pins::PIN_TIER0)].name)
            .ok()
            .flatten()
        else {
            // SAFETY: as above.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "tier0_handle_mismatch",
            ));
        };
        if tier0.image().base() != modules::live().base(pins::PIN_TIER0) {
            // SAFETY: as above.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "tier0_handle_mismatch",
            ));
        }
        let Some(load_kv3) = tier0.symbol(reply::PET_LOADKV3_SYM) else {
            // SAFETY: as above.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "loadkv3_export_missing",
            )); // :1041-1047
        };
        // SAFETY: the export is tier0's text overload of `LoadKV3`, declared with this shape at
        // `mcp_bridge_pet.inc:65`.
        let load_kv3: LoadKv3Fn = unsafe { core::mem::transmute(load_kv3) };
        let Ok(text_c) = std::ffi::CString::new(text) else {
            // The caller rejected NUL-containing text? No: `text` is the doc's bytes, and the C
            // hands them to `LoadKV3` as a C string. A NUL cannot appear (the unescaper refuses
            // `\u0000`), so this is unreachable.
            // SAFETY: `ctx` is constructed.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "kv3_load_failed",
            ));
        };
        let format = pin_addr(pins::PETG_KV3ID_VPCF66) as *const c_void; // `:1049`
        let empty = c"";
        let mut err: *const c_char = core::ptr::null();
        // SAFETY: every argument is the one the C passes (`:1049-1050`): the parsed context's
        // root, the error out-parameter, the text, the pinned `vpcf66` KV3ID, an empty binding
        // name, and flags 0.
        let ok = unsafe {
            load_kv3(
                kv,
                &mut err,
                text_c.as_ptr(),
                format,
                empty.as_ptr(),
                reply::PET_LOADKV3_FLAGS,
            )
        };
        if ok == 0 {
            // SAFETY: `ctx` is constructed.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            let mut eb = vec![0u8; 100];
            // `:1055`: an unreadable error text becomes "?"; a cut one is its prefix.
            let mut sanitized = if md_str(err, &mut eb) == StrRead::Unreadable {
                Vec::from(&b"?"[..])
            } else {
                reply::cstr(&eb).to_vec()
            };
            // `:1056-1057`: tier0's message quotes raw input bytes - keep the reply UTF-8 by
            // replacing every byte outside 0x20..0x7e, one '?' per byte as the C does.
            for byte in &mut sanitized {
                if *byte >= 0x80 || *byte < 0x20 {
                    *byte = b'?';
                }
            }
            let sanitized = String::from_utf8_lossy(&sanitized).into_owned();
            log::line(&reply::log_refusal("apply", "kv3_load_failed", sanitized.as_bytes()));
            return Err(reply::unloadable("kv3_load_failed", sanitized.as_bytes()));
        }
        // SAFETY: `kv` is the constructed context's root, as `LoadKV3` filled it.
        let scope = unsafe { (fns.type_scope)() };
        if scope.is_null() {
            // SAFETY: `ctx` is constructed.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "no_type_scope",
            )); // :1067-1073
        }
        let Ok(label_c) = std::ffi::CString::new(label) else {
            // The label is printable ASCII without quotes or backslashes (`reply::apply_params`),
            // so it has no NUL.
            // SAFETY: `ctx` is constructed.
            unsafe { (fns.kv3_ctx_dtor)(ctx) };
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "kv3_root_null",
            ));
        };
        // SAFETY: `kv` and `scope` are the objects the C passes (`:1074-1087`); `live.doc` /
        // `live.root` / `live.session` are the identity-checked chain `live_doc` produced.
        unsafe {
            (fns.strip_to_schema)(kv, scope); // :1074
            (fns.begin_undo_step)(live.doc, label_c.as_ptr(), 1); // :1075
            (fns.root_load_from_kv3)(live.root, kv); // :1076
            (fns.filter_functions)(live.root, (fns.behavior_version)(live.root)); // :1077
            clear_selection(live.doc); // :1078
            // `:1079`: `doc->m_unk102` is the document's own cache block.
            (fns.refresh_caches)(live.root, addr_of_mut!((*live.doc.cast::<CPetDoc>()).m_unk102).cast());
            // `:1081-1083`: the callee reads [3] as the session.
            (fns.notify_func_list)(func_list(live.session));
            (fns.session_on_doc_modified)(live.session);
            (fns.kv3_ctx_dtor)(ctx); // :1086
        }
        Ok(())
    }

    /// `PETFN_DOC_SAVETOFILE` via `pet.text` (`mcp_bridge_pet.inc:908`). `Err(Some(text))` is a
    /// failure whose `err` string was readable, `Err(None)` one whose was not.
    pub fn save_to_file(live: &LiveDoc, path: &str) -> Result<(), Option<String>> {
        let fns = pet_fns();
        let Ok(path_c) = std::ffi::CString::new(path) else {
            return Err(None);
        };
        let mut err: *const c_char = core::ptr::null(); // CUtlString {char *} = {0}
        // SAFETY: `live.doc` is the identity-checked document; the entry point takes the path,
        // the error out-parameter and the 64-byte zero callback block the C passes
        // (`bridge/bridge_pins.h:134`, `mcp_bridge_pet.inc:811-812`).
        let ok = unsafe {
            (fns.save_to_file)(
                live.doc,
                path_c.as_ptr(),
                &mut err,
                SAVE_CB.0.get().cast(),
            )
        };
        if ok != 0 {
            return Ok(());
        }
        if err.is_null() || !mem_ok(err.cast()) {
            return Err(None); // the C only folds the text in when it is readable (`:911`)
        }
        // `md_str(errp, msg + n, sizeof msg - n)` (`:915`) with `n` = `slen("save_to_file_failed: ")`:
        // the callee's text gets the *remaining* bytes of the C's 192-byte `msg`, not a fresh 1024.
        Err(Some(read_text(err, SAVE_ERROR_CAP)))
    }

    // -----------------------------------------------------------------------------------------
    // pet.load (`mcp_bridge_pet.inc:421-458`)
    // -----------------------------------------------------------------------------------------

    /// C `g_petLoadTried` / `g_petLoadIdx` / `g_petLoadOk` (`:101-103`). Main thread only.
    static LOAD_TRIED: AtomicBool = AtomicBool::new(false);
    static LOAD_IDX: AtomicI32 = AtomicI32::new(-1);
    static LOAD_OK: AtomicBool = AtomicBool::new(false);

    /// `op_pet_load` (`mcp_bridge_pet.inc:421-458`).
    pub fn load(out: &mut Buf) -> Result<(), OpErr> {
        let framework = mod_iface(pins::PIN_TF2, TOOLFRAMEWORK2_INTERFACE_VERSION); // :422
        if !obj_is(
            framework,
            pins::PIN_TF2,
            vt_rva(pins::TFV_FRAMEWORK_VT),
        ) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "framework_unavailable",
            )); // :423-426
        }
        if pet_mapped() {
            // :427-431: already mapped, nothing is called.
            reply::load_already(out, module_state_name());
            return Ok(());
        }
        // `if (!g_petLoadTried) { g_petLoadTried = 1; ... }` (`:433`).
        if LOAD_TRIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // `u64 idx = 0xFF` - the framework stores a 1-byte index (`:435-436`); the slot's
            // declaration is `void *(*GetToolIndexByName)(IToolFramework2 *, u64 *, const char *)`
            // (`bridge/s2sdk.h:143`), so the pointer is a `u64 *` and the byte is its low one.
            let mut idx: u64 = 0xFF;
            type FindToolFn =
                unsafe extern "C" fn(*mut IToolFramework2, *mut u64, *const c_char) -> *mut u8;
            // SAFETY: `framework` passed `obj_is` against the pinned ToolFramework2 vtable and
            // the slot is `S2_SLOT_IToolFramework2_FindTool` (`bridge/s2sdk.h:140`); the name
            // is a live C string and `idx` a writable 8-byte index slot.
            let tool = unsafe {
                let find: FindToolFn = pet_vfn(framework, IToolFramework2_slots::FindTool);
                let name = c"pet";
                let _ = find(framework.cast(), &mut idx, name.as_ptr());
                (idx & 0xFF) as u8
            };
            if tool == 0xFF {
                // :438-442: LoadToolSystem would be a wild index.
                LOAD_IDX.store(-1, Ordering::Release);
                return Err(OpErr::new(
                    -32017,
                    "PrerequisiteFailed",
                    "tool_index_unknown",
                ));
            }
            type LoadToolFn =
                unsafe extern "C" fn(*mut IToolFramework2, u8) -> *mut c_void;
            // SAFETY: as above; the slot is `S2_SLOT_IToolFramework2_LoadTool`
            // (`bridge/s2sdk.h:141`) and `tool` is the index the framework just confirmed.
            let rc = unsafe {
                let load: LoadToolFn = pet_vfn(framework, IToolFramework2_slots::LoadTool);
                load(framework.cast(), tool)
            };
            LOAD_IDX.store(i32::from(tool), Ordering::Release); // :444
            LOAD_OK.store(!rc.is_null() && pet_mapped(), Ordering::Release); // :445
            log::line(&format!("pet: LoadToolSystem rc {}", rc as usize)); // :446
        }
        let tool = LOAD_IDX.load(Ordering::Acquire);
        if tool < 0 {
            // :448-451: memoized - the tool index was never known.
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "tool_index_unknown",
            ));
        }
        reply::load_result(
            out,
            LOAD_OK.load(Ordering::Acquire),
            i64::from(tool),
            module_state_name(),
        ); // :452-456
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // pet.functionTypes (`mcp_bridge_pet.inc:539-633`)
    // -----------------------------------------------------------------------------------------

    /// Read one registry row (`pet_type_row`, `mcp_bridge_pet.inc:539-581`).
    fn type_row(def: *mut c_void) -> TypeRow {
        if def.is_null() || !mem_ok(def.cast()) {
            return TypeRow::Unreadable; // :542
        }
        // SAFETY: `def` is readable; the vptr is at offset 0 and must itself be readable
        // (`:542`).
        let vptr = unsafe { def.cast::<*const c_void>().read_unaligned() };
        if vptr.is_null() || !mem_ok(vptr.cast()) {
            return TypeRow::Unreadable;
        }
        type GetNameFn =
            unsafe extern "C" fn(*mut IParticleFunctionDefinition) -> *const c_char;
        // SAFETY: the vptr is readable and the slot is the generated
        // `S2_SLOT_IParticleFunctionDefinition_GetName`.
        let (read, name) = unsafe {
            let f: GetNameFn = pet_vfn(def, IParticleFunctionDefinition_slots::GetName);
            read_str(f(def.cast()), CLASS_CAP)
        };
        if read != StrRead::Whole {
            return TypeRow::Unreadable; // :543-544
        }
        // `GetSchemaBinding(def, out8)` writes the pointer into the 8-byte local and returns the
        // local's address (`:545-549`).
        type GetSchemaBindingFn = unsafe extern "C" fn(
            *mut IParticleFunctionDefinition,
            *mut *mut CSchemaClassInfo,
        ) -> *mut *mut CSchemaClassInfo;
        // SAFETY: as above; the slot is the generated
        // `S2_SLOT_IParticleFunctionDefinition_GetSchemaBinding`.
        let ret = unsafe {
            let f: GetSchemaBindingFn =
                pet_vfn(def, IParticleFunctionDefinition_slots::GetSchemaBinding);
            let mut out8: *mut CSchemaClassInfo = core::ptr::null_mut();
            f(def.cast(), &mut out8)
        };
        if ret.is_null() || !mem_ok(ret.cast()) {
            return TypeRow::Unreadable; // :550
        }
        // SAFETY: `ret` is readable (`:550`).
        let binding = unsafe { ret.read_unaligned() };
        if binding.is_null() || !mem_ok(binding.cast()) {
            return TypeRow::Unreadable;
        }
        // SAFETY: `binding` is a readable `SchemaClassInfoData_t` (`:551`).
        let class: &SchemaClassInfoData_t = unsafe { &*(binding.cast::<CSchemaClassInfo>()) }.base_ref();
        let (read, class_name) = read_str(class.m_pName, CLASS_CAP);
        if read != StrRead::Whole {
            return TypeRow::Unreadable; // :552
        }
        let count = u32::from(class.m_nNumMetadata);
        if count > 4096 {
            return TypeRow::Unreadable; // :554
        }
        let metadata = match read_metadata(class.m_pMetadata, count) {
            Some(names) => names,
            None => return TypeRow::Unreadable, // :555-557
        };
        TypeRow::Row {
            name: String::from_utf8_lossy(&name).into_owned(),
            class_name: String::from_utf8_lossy(&class_name).into_owned(),
            metadata,
        }
    }

    /// `op_pet_function_types` (`mcp_bridge_pet.inc:589-633`).
    pub fn function_types(out: &mut Buf) -> Result<(), OpErr> {
        // The C reads `g_modBase[PIN_PET]` unguarded (`:591`); the gate has already proved the
        // module verified, so the base is non-zero. Refusing a zero base keeps a pathological
        // call from faulting on address 0 without changing any reachable answer.
        if modules::live().base(pins::PIN_PET) == 0 {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "particle_mgr_null",
            ));
        }
        // SAFETY: `PETG_PARTICLE_MGR` is a pinned global inside the verified pet.dll image
        // (`bridge/bridge_pins.h:164`).
        let mgr = unsafe {
            (pin_addr(pins::PETG_PARTICLE_MGR) as *const *mut c_void).read_unaligned()
        };
        if mgr.is_null() || !mem_ok(mgr.cast()) {
            // 0 before pet's Connect (`:592-596`).
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "particle_mgr_null",
            ));
        }
        // `:597-605`: probe the manager's vtable and both slots before the first call.
        let vptr = unsafe { mgr.cast::<*const c_void>().read_unaligned() };
        if vptr.is_null()
            || !mem_ok(vptr.cast())
            || !mem_ok(vptr.cast::<*const c_void>().wrapping_add(IParticleSystemMgr_slots::GetFunctionDefinitionCount).cast())
            || !mem_ok(vptr.cast::<*const c_void>().wrapping_add(IParticleSystemMgr_slots::GetFunctionDefinition).cast())
        {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "particle_mgr_unreadable",
            ));
        }
        type CountFn = unsafe extern "C" fn(*mut IParticleSystemMgr, c_int) -> c_int;
        type GetFn =
            unsafe extern "C" fn(*mut IParticleSystemMgr, c_int, c_int) -> *mut IParticleFunctionDefinition;
        let mut counts = [0i32; reply::FUNCTION_TYPES];
        for (t, count) in counts.iter_mut().enumerate() {
            // SAFETY: the vptr and the slot were probed above; the slot is the generated
            // `S2_SLOT_IParticleSystemMgr_GetFunctionDefinitionCount`.
            *count = unsafe {
                let f: CountFn =
                    pet_vfn(mgr, IParticleSystemMgr_slots::GetFunctionDefinitionCount);
                f(mgr.cast(), t as c_int)
            };
            if !(0..=4096).contains(count) {
                // `:610-613`: the C checks each count as it is read and returns before reading
                // the next one, so no later count call is made either.
                return Err(OpErr::new(
                    -32017,
                    "PrerequisiteFailed",
                    "function_count_unusable",
                ));
            }
        }
        let mut rows: [Vec<TypeRow>; reply::FUNCTION_TYPES] =
            core::array::from_fn(|_| Vec::new());
        for (t, type_rows) in rows.iter_mut().enumerate() {
            for i in 0..counts[t] {
                // SAFETY: as above; the slot is the generated
                // `S2_SLOT_IParticleSystemMgr_GetFunctionDefinition`.
                let def = unsafe {
                    let f: GetFn = pet_vfn(mgr, IParticleSystemMgr_slots::GetFunctionDefinition);
                    f(mgr.cast(), t as c_int, i)
                };
                let row = type_row(def.cast());
                let unreadable = matches!(row, TypeRow::Unreadable);
                type_rows.push(row);
                if unreadable {
                    // `:624-627`: the C rolls the whole reply back and returns on the first bad
                    // row - it walks no further definition of this or any later type.
                    return Err(OpErr::new(
                        -32017,
                        "PrerequisiteFailed",
                        "function_definition_unreadable",
                    ));
                }
            }
        }
        reply::function_types_json(out, &counts, &rows)
    }

    // -----------------------------------------------------------------------------------------
    // pet.schema (`mcp_bridge_pet.inc:635-783`)
    // -----------------------------------------------------------------------------------------

    /// `pet_schema_metadata` (`mcp_bridge_pet.inc:699-717`): the class's static metadata names.
    fn read_metadata(
        md: *mut SchemaMetadataEntryData_t,
        count: u32,
    ) -> Option<Vec<String>> {
        if count > 4096 {
            return None; // :701
        }
        if count == 0 {
            return Some(Vec::new()); // `:703-715`: the `if (nm)` arm
        }
        let entry_size = core::mem::size_of::<SchemaMetadataEntryData_t>();
        if md.is_null()
            || !mem_ok(md.cast())
            // `md + S2_SIZEOF_SchemaMetadataEntryData_t * nm - 1` (`:705`).
            || !mem_ok(unsafe { md.cast::<u8>().add(entry_size * count as usize - 1) }.cast())
        {
            return None;
        }
        let mut names = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: the whole entry range was probed above.
            let entry = unsafe { md.cast::<u8>().add(entry_size * i) }.cast::<SchemaMetadataEntryData_t>();
            // SAFETY: as above; `m_pName` is the entry's first member
            // (`bridge/s2sdk_gen.h:143`).
            let name = unsafe { entry.read_unaligned().m_pName };
            let (read, bytes) = read_str(name, CLASS_CAP);
            if read != StrRead::Whole {
                return None; // :708-710
            }
            names.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        Some(names)
    }

    /// The borrowed `SchemaClassInfoData_t` of a `CSchemaClassInfo` (`CSchemaClassInfo`'s only
    /// base, generated).
    trait ClassInfoRef {
        fn base_ref(&self) -> &SchemaClassInfoData_t;
    }

    impl ClassInfoRef for CSchemaClassInfo {
        fn base_ref(&self) -> &SchemaClassInfoData_t {
            &self.base
        }
    }

    /// `pet_schema_fields` (`mcp_bridge_pet.inc:639-662`) plus the walk's own field collection.
    fn walk_fields(
        class: &SchemaClassInfoData_t,
        declared_in: &str,
        depth: i32,
        out: &mut Vec<SchemaField>,
    ) -> Option<()> {
        if depth > reply::SCHEMA_DEPTH {
            return Some(()); // :666: stop, don't fail
        }
        let count = u32::from(class.m_nNumFields);
        if count != 0 {
            let fields = class.m_pFields;
            let entry_size = core::mem::size_of::<SchemaClassFieldData_t>();
            if fields.is_null()
                || !mem_ok(fields.cast())
                // `fields + nf - 1` (`:643`).
                || !mem_ok(unsafe { fields.cast::<u8>().add(entry_size * count as usize - 1) }.cast())
            {
                return None;
            }
            for i in 0..count as usize {
                // SAFETY: the whole field range was probed above.
                let field = unsafe {
                    fields.cast::<u8>().add(entry_size * i).cast::<SchemaClassFieldData_t>()
                };
                // SAFETY: as above (`:645`).
                let field = unsafe { field.read_unaligned() };
                let (read, name) = read_str(field.m_pName, CLASS_CAP);
                if read != StrRead::Whole {
                    return None; // :648
                }
                let type_name = read_field_type(field.m_pType);
                out.push(SchemaField {
                    name: String::from_utf8_lossy(&name).into_owned(),
                    type_name,
                    declared_in: declared_in.to_string(),
                });
            }
        }
        let bases = class.m_nNumBaseClasses;
        if bases == 0 {
            return Some(()); // :671
        }
        let list = class.m_pBaseClasses;
        let entry_size = core::mem::size_of::<SchemaBaseClassInfoData_t>();
        if list.is_null()
            || !mem_ok(list.cast())
            // `bases + nb - 1` (`:673`).
            || !mem_ok(unsafe { list.cast::<u8>().add(entry_size * usize::from(bases) - 1) }.cast())
        {
            return None;
        }
        for j in 0..usize::from(bases) {
            // SAFETY: the whole base range was probed above (`:674-675`).
            let base = unsafe {
                list.cast::<u8>().add(entry_size * j).cast::<SchemaBaseClassInfoData_t>()
            };
            let child = unsafe { base.read_unaligned().m_pClass };
            if child.is_null() || !mem_ok(child.cast()) {
                return None;
            }
            // `pet_schema_walk(b, depth + 1)`'s own prologue (`:665-666`): the base must be a
            // readable class, then `depth > 8` stops before the base's name is read. At depth 8
            // the C therefore never touches `b->m_pName` (the loop still visits the remaining
            // base entries and each call returns at the depth check).
            if depth + 1 > reply::SCHEMA_DEPTH {
                continue;
            }
            // SAFETY: `m_pClass` is a readable `SchemaClassInfoData_t` (`:675`).
            let child = unsafe { &*(child.cast::<CSchemaClassInfo>()) }.base_ref();
            let (read, name) = read_str(child.m_pName, CLASS_CAP);
            if read != StrRead::Whole {
                return None; // :668
            }
            let name = String::from_utf8_lossy(&name).into_owned();
            walk_fields(child, &name, depth + 1, out)?;
        }
        Some(())
    }

    /// `f->m_pType`'s `CSchemaType::m_sTypeName` (`mcp_bridge_pet.inc:649-654`): both hops are
    /// probed, and a cut or unreadable name is `null`.
    fn read_field_type(ty: *mut CSchemaType) -> Option<String> {
        if ty.is_null() || !mem_ok(ty.cast()) {
            return None;
        }
        // SAFETY: `ty` is readable; the C probes the `m_sTypeName.m_pString` word before reading
        // it (`:651-653`).
        let pp = unsafe { addr_of!((*ty).m_sTypeName.m_pString) };
        if !mem_ok(pp.cast()) {
            return None;
        }
        // SAFETY: probed above.
        let p = unsafe { pp.read_unaligned() };
        let (read, bytes) = read_str(p, CLASS_CAP);
        (read == StrRead::Whole).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `pet_schema_bases` (`mcp_bridge_pet.inc:682-697`): the flattened base chain in the field
    /// walk's order, excluding the class itself.
    fn walk_bases(
        class: &SchemaClassInfoData_t,
        depth: i32,
        out: &mut Vec<String>,
    ) -> Option<()> {
        if depth >= reply::SCHEMA_DEPTH {
            return Some(()); // :683
        }
        let bases = class.m_nNumBaseClasses;
        if bases == 0 {
            return Some(());
        }
        let list = class.m_pBaseClasses;
        let entry_size = core::mem::size_of::<SchemaBaseClassInfoData_t>();
        if list.is_null()
            || !mem_ok(list.cast())
            || !mem_ok(unsafe { list.cast::<u8>().add(entry_size * usize::from(bases) - 1) }.cast())
        {
            return None; // :687
        }
        for j in 0..usize::from(bases) {
            // SAFETY: the range was probed above (`:688-690`).
            let base = unsafe {
                list.cast::<u8>().add(entry_size * j).cast::<SchemaBaseClassInfoData_t>()
            };
            let child = unsafe { base.read_unaligned().m_pClass };
            if child.is_null() || !mem_ok(child.cast()) {
                return None;
            }
            // SAFETY: readable `SchemaClassInfoData_t`.
            let child = unsafe { &*(child.cast::<CSchemaClassInfo>()) }.base_ref();
            let (read, name) = read_str(child.m_pName, CLASS_CAP);
            if read != StrRead::Whole {
                return None; // :691
            }
            out.push(String::from_utf8_lossy(&name).into_owned());
            walk_bases(child, depth + 1, out)?;
        }
        Some(())
    }

    /// `op_pet_schema`'s read (`mcp_bridge_pet.inc:738-782`).
    pub fn schema_read(class_name: &str) -> Result<SchemaRead, OpErr> {
        let system = mod_iface(pins::PIN_SCHEMA, SCHEMASYSTEM_INTERFACE_VERSION); // :738
        if !obj_is(
            system,
            pins::PIN_SCHEMA,
            vt_rva(pins::SSV_SCHEMASYSTEM_VT),
        ) {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "SchemaSystem_001 is not the pinned CSchemaSystem",
            )); // :739-742
        }
        let Ok(particles) = std::ffi::CString::new("particles.dll") else {
            return Err(OpErr::new(
                -32023,
                "SchemaUnavailable",
                "particles.dll has not registered its schema scope yet",
            ));
        };
        type FindScopeFn = unsafe extern "C" fn(
            *mut ISchemaSystem,
            *const c_char,
            *mut *const c_char,
        ) -> *mut ISchemaSystemTypeScope;
        // SAFETY: `system` passed `obj_is`; the slot is the generated
        // `S2_SLOT_ISchemaSystem_FindTypeScopeForModule`. The binding-name out-parameter is null
        // as the C passes 0 (`:743`).
        let scope = unsafe {
            let f: FindScopeFn = pet_vfn(system, ISchemaSystem_slots::FindTypeScopeForModule);
            f(system.cast(), particles.as_ptr(), core::ptr::null_mut())
        };
        if !obj_is(
            scope.cast(),
            pins::PIN_SCHEMA,
            vt_rva(pins::SSV_TYPESCOPE_VT),
        ) {
            return Err(OpErr::new(
                -32023,
                "SchemaUnavailable",
                "particles.dll has not registered its schema scope yet",
            )); // :744-747
        }
        type IsGlobalFn = unsafe extern "C" fn(*mut ISchemaSystemTypeScope) -> u8;
        // SAFETY: `scope` passed `obj_is`; the slot is the generated
        // `S2_SLOT_ISchemaSystemTypeScope_IsGlobalScope`. The C's typedef is
        // `unsigned char (*TsIsGlobalFn)(void *)` - "bool in AL: never read it as int"
        // (`bridge/mcp_bridge_game.inc:15`).
        if unsafe {
            let f: IsGlobalFn = pet_vfn(scope.cast(), ISchemaSystemTypeScope_slots::IsGlobalScope);
            f(scope) != 0
        } {
            return Err(OpErr::new(
                -32023,
                "SchemaUnavailable",
                "particles.dll resolved to the global type scope",
            )); // :748-751
        }
        type FindRawClassFn = unsafe extern "C" fn(
            *mut ISchemaSystemTypeScope,
            *const c_char,
        ) -> *mut CSchemaClassInfo;
        let Ok(name) = std::ffi::CString::new(class_name) else {
            return Ok(SchemaRead::NotFound);
        };
        // SAFETY: as above; the slot is the generated
        // `S2_SLOT_ISchemaSystemTypeScope_FindRawClassBinding`, and `name` outlives the call.
        let found = unsafe {
            let f: FindRawClassFn =
                pet_vfn(scope.cast(), ISchemaSystemTypeScope_slots::FindRawClassBinding);
            f(scope, name.as_ptr())
        };
        // `:754-758`: the name must compare equal, not merely be found.
        if found.is_null() || !mem_ok(found.cast()) {
            return Ok(SchemaRead::NotFound);
        }
        // SAFETY: `found` is readable; the class data starts at it.
        let class = unsafe { &*found.cast::<CSchemaClassInfo>() }.base_ref();
        // `seq(cls->m_pName, clsname)`: exact C-string equality. A cut read cannot equal a
        // name of at most 127 characters, and an unreadable one is "" either way.
        let (name_read, name_bytes) = read_str(class.m_pName, CLASS_CAP);
        if name_read != StrRead::Whole || reply::cstr(&name_bytes) != class_name.as_bytes() {
            return Ok(SchemaRead::NotFound);
        }
        let class_name_text = String::from_utf8_lossy(&name_bytes).into_owned();
        let project = {
            let (read, bytes) = read_str(class.m_pProjectName, CLASS_CAP);
            (read == StrRead::Whole).then(|| String::from_utf8_lossy(&bytes).into_owned())
        };
        let mut bases = Vec::new();
        if walk_bases(class, 0, &mut bases).is_none() {
            return Ok(SchemaRead::Unreadable); // :772
        }
        let mut fields = Vec::new();
        if walk_fields(class, &class_name_text, 0, &mut fields).is_none() {
            return Ok(SchemaRead::Unreadable); // :774
        }
        let Some(metadata) = read_metadata(class.m_pMetadata, u32::from(class.m_nNumMetadata))
        else {
            return Ok(SchemaRead::Unreadable); // :776
        };
        Ok(SchemaRead::Found(ClassSchema {
            size: class.m_nSizeOf,
            project,
            bases,
            fields,
            metadata,
        }))
    }

    // -----------------------------------------------------------------------------------------
    // The host file facts (`mcp_bridge_pet.inc:355-414`, `:822-832`)
    // -----------------------------------------------------------------------------------------

    /// `LOCALAPPDATA`, with the C's fallback (`mcp_bridge_pet.inc:826-828`).
    pub fn local_app_data() -> String {
        match std::env::var("LOCALAPPDATA") {
            Ok(value) if !value.is_empty() && value.len() < 260 => value,
            _ => String::from("C:\\users\\steamuser\\AppData\\Local"),
        }
    }

    /// `pet_read_vpcf` (`mcp_bridge_pet.inc:263-299`), the C's own calls.
    ///
    /// `CreateFileA` with the C's flags opening the path's raw bytes as an ANSI path (a code-page
    /// name is not a Rust `str`, so it is never decoded/re-encoded on the way here), then
    /// `GetFileType` (`FILE_TYPE_DISK` only: a pipe or device would block `ReadFile` on the main
    /// thread forever), then `GetFileSize` with its 4 GB sentinel and the C's `hi` check, then a
    /// read of one byte more than the size to detect a file that grew under us. The read itself
    /// goes through `std::fs::File`, which is the same `ReadFile`; the handle ownership is the
    /// only difference, and no answer changes.
    pub fn read_vpcf(path: &[u8]) -> HostRead {
        use std::os::windows::io::FromRawHandle as _;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileA, GetFileSize, GetFileType, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, OPEN_EXISTING,
        };
        use windows_sys::Win32::Foundation::GENERIC_READ;

        let Ok(path) = std::ffi::CString::new(path) else {
            return HostRead::Refused("read_failed"); // a path with an interior NUL cannot be opened
        };
        // SAFETY: the C's `CreateFileA(path, GENERIC_READ, FILE_SHARE_READ|WRITE|DELETE, 0,
        // OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0)` (`:267-269`); `path` is a live C string and
        // the returned handle is checked before use.
        let handle = unsafe {
            CreateFileA(
                path.as_ptr().cast(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return HostRead::Refused("read_failed"); // :270
        }
        // SAFETY: `handle` is the live handle just opened; the C probes it the same way (`:271`).
        if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
            // SAFETY: `handle` is still ours and live.
            unsafe { CloseHandle(handle) };
            return HostRead::Refused("read_failed"); // :271-275
        }
        let mut hi = 0u32;
        // SAFETY: as above; `hi` is the high half out-parameter (`:277`).
        let size = unsafe { GetFileSize(handle, &mut hi) };
        // `size == 0xFFFFFFFFu` is the error *or* 4 GB (`:278`); `hi` refuses > 4 GB (`:279`).
        if size == u32::MAX {
            // SAFETY: as above.
            unsafe { CloseHandle(handle) };
            return HostRead::Refused("read_failed"); // :278
        }
        if hi != 0 || u64::from(size) > reply::PET_VPCF_MAX {
            // SAFETY: as above.
            unsafe { CloseHandle(handle) };
            return HostRead::Refused("too_large"); // :279
        }
        // SAFETY: `handle` is a valid open file handle with read access and ownership is handed
        // to the `File`, so it is closed exactly once.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        use std::io::Read as _;
        // One byte more than the file, as the C's `size + 1` allocation (`:280`, `:284-289`).
        let mut bytes = Vec::with_capacity(size as usize + 1);
        if file.take(u64::from(size) + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > u64::from(size) {
            return HostRead::Refused("read_failed"); // :291-295
        }
        HostRead::Bytes(bytes)
    }
}

// ---------------------------------------------------------------------------------------------
// The non-Windows half: the same signatures, no pet.dll
// ---------------------------------------------------------------------------------------------

#[cfg(not(windows))]
pub(crate) mod platform {
    use crate::ops::OpErr;

    use super::super::reply::{Buf, SchemaRead};
    use super::{ActiveDoc, HostRead, LiveDoc, LiveTail, PathText, unsupported};

    /// Every reader answers [`unsupported`]: pet.dll is a Windows DLL.
    pub fn load(_out: &mut Buf) -> Result<(), OpErr> {
        Err(unsupported())
    }

    pub fn active_session() -> Result<*mut core::ffi::c_void, &'static str> {
        Err("pet_not_loaded")
    }

    pub fn active_doc() -> ActiveDoc {
        ActiveDoc::NoSession("pet_not_loaded")
    }

    pub fn live_doc() -> Result<LiveDoc, OpErr> {
        Err(unsupported())
    }

    pub fn is_modified(_session: *mut core::ffi::c_void) -> bool {
        false
    }

    pub fn is_new(_session: *mut core::ffi::c_void) -> bool {
        false
    }

    pub fn get_path(_session: *mut core::ffi::c_void) -> PathText {
        PathText {
            text: Vec::new(),
            whole: false,
        }
    }

    pub fn reload_document(_session: *mut core::ffi::c_void) {}

    pub fn save_document(_session: *mut core::ffi::c_void, _path: &[u8]) -> bool {
        false
    }

    pub fn set_unmodified(_session: *mut core::ffi::c_void) {}

    pub fn resolve_pet() -> Option<*mut core::ffi::c_void> {
        None
    }

    pub fn open_file(_app: *mut core::ffi::c_void, _path: &str) -> *mut core::ffi::c_void {
        core::ptr::null_mut()
    }

    pub fn document_presence() -> (bool, bool) {
        (false, false)
    }

    pub fn has_entries(_hist: *mut core::ffi::c_void) -> bool {
        false
    }

    pub fn live_tail(_live: &LiveDoc) -> LiveTail {
        LiveTail {
            dirty: false,
            can_undo: false,
            can_redo: false,
            arrays: core::array::from_fn(|_| super::super::reply::TypeArray::Children {
                count: 0,
                classes: Vec::new(),
            }),
        }
    }

    pub fn undo_redo(_live: &LiveDoc, _redo: bool) {}

    pub fn apply_text(
        _live: &LiveDoc,
        _text: &[u8],
        _label: &str,
    ) -> Result<(), OpErr> {
        Err(unsupported())
    }

    pub fn save_to_file(_live: &LiveDoc, _path: &str) -> Result<(), Option<String>> {
        Err(None)
    }

    pub fn function_types(_out: &mut Buf) -> Result<(), OpErr> {
        Err(unsupported())
    }

    pub fn schema_read(_class_name: &str) -> Result<SchemaRead, OpErr> {
        Err(unsupported())
    }

    pub fn local_app_data() -> String {
        String::new()
    }

    pub fn read_vpcf(_path: &[u8]) -> HostRead {
        HostRead::Refused("read_failed")
    }
}

// ---------------------------------------------------------------------------------------------
// The host file facts shared by both targets (`mcp_bridge_pet.inc:355-414`)
// ---------------------------------------------------------------------------------------------

/// `<app>`, into the C's `char app[1024]` (`mcp_bridge_pet.inc:379`).
///
/// `editor_app_root` (`:368-373`) reads `GetModuleFileNameA(0, exe, 1024)` and hands the name to
/// `kv3_app_root` with `sizeof app`, so the *app* buffer is 1024 bytes - much larger than the
/// 512-byte cap of `kv3_check`'s host-test wrapper. `std::env::current_exe` is the same question
/// through the loader.
#[must_use]
pub fn app_root() -> Option<String> {
    let exe = std::env::current_exe().ok()?.to_string_lossy().into_owned();
    if exe.is_empty() || exe.len() >= 1024 {
        return None; // `if (!n || n >= sizeof exe) return 0;` (`:371`)
    }
    let mut app = [0u8; APP_CAP];
    kv3_check::app_root_into(&exe, &mut app).then(|| filled(&app).to_string())
}

/// `kv3_addon_name(GetCommandLineA(), addon, sizeof addon)` (`mcp_bridge_pet.inc:389`) into the
/// C's `char addon[64]`: an addon name is a content directory name, so 63 characters is the C's
/// whole budget.
///
/// The C reads the raw command line. This crate's `windows-sys` feature set has no
/// `Win32_System_Environment` (only `Win32_Storage_FileSystem` may be added for this lane), so
/// the CRT-parsed arguments are joined with spaces instead: `-addon <name>`, `-addon=<name>` and
/// `-addon "<name>"` all parse the same, and those are the shapes `runtime.sh` passes. Noted in
/// `bridge/protocol-diff-m3-pet.md`.
#[must_use]
pub fn addon_name() -> Option<String> {
    let command_line = command_line();
    let mut addon = [0u8; ADDON_CAP];
    kv3_check::addon_name_into(&command_line, &mut addon).then(|| filled(&addon).to_string())
}

/// The bytes before the first NUL of a buffer `kv3_*_into` filled, as a `&str`: the helpers copy
/// a `&str` prefix and cut at ASCII separators, so the bytes are valid UTF-8.
fn filled(buf: &[u8]) -> &str {
    let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..len])
        .expect("the kv3_check *_into helpers copy a &str prefix at an ASCII separator")
}

/// The C's `char app[1024]` (`mcp_bridge_pet.inc:379`).
pub const APP_CAP: usize = 1024;
/// The C's `char addon[64]` (`mcp_bridge_pet.inc:379`).
pub const ADDON_CAP: usize = 64;
/// The C's `char host[2048]` (`mcp_bridge_pet.inc:379`).
pub const HOST_CAP: usize = 2048;

/// `pet_open_gate` (`mcp_bridge_pet.inc:378-414`) over the process facts and one file reader,
/// with the C's buffer sizes: `app[1024]`, `addon[64]`, `host[2048]`.
///
/// The path-shape refusals report the caller's addon-relative path; everything from the host-path
/// derivation on reports the host file. `read` is `pet_read_vpcf` in production and a plain
/// closure in the host tests; `Err` is the C's `why`.
#[must_use]
pub fn open_gate_with<F>(
    path: &str,
    exe: Option<&str>,
    cmdline: Option<&str>,
    read: F,
) -> Option<reply::Refused>
where
    F: FnOnce(&[u8]) -> Result<Vec<u8>, &'static str>,
{
    let mut app = [0u8; APP_CAP];
    let Some(exe) = exe else {
        return Some(reply::Refused::new("open", "no_app_root", path.as_bytes())); // :384-388
    };
    if !kv3_check::app_root_into(exe, &mut app) {
        return Some(reply::Refused::new("open", "no_app_root", path.as_bytes())); // :384-388
    }
    let mut addon = [0u8; ADDON_CAP];
    let Some(cmdline) = cmdline else {
        return Some(reply::Refused::new("open", "no_addon", path.as_bytes())); // :389-393
    };
    if !kv3_check::addon_name_into(cmdline, &mut addon) {
        return Some(reply::Refused::new("open", "no_addon", path.as_bytes())); // :389-393
    }
    let mut host = [0u8; HOST_CAP];
    // `:394-400`: unreachable with the caps above (app <= 1023, addon <= 63, path <= 512, all
    // bounded by the caller), and the C reports it as a failed read.
    if !kv3_check::host_path_into(filled(&app), filled(&addon), path, &mut host) {
        return Some(reply::Refused::new("open", "read_failed", path.as_bytes()));
    }
    let host = reply::cstr(&host).to_vec();
    let read = read(&host);
    let file = match &read {
        Err(why) => reply::FileRead::Refused(why), // :401-405
        Ok(bytes) => reply::FileRead::Bytes(bytes),
    };
    reply::open_gate(
        path.as_bytes(),
        Some(filled(&app)),
        Some(filled(&addon)),
        Some(&host),
        Some(&file),
    )
}

/// `pet_open_gate` (`mcp_bridge_pet.inc:378-414`): the two process facts, then [`open_gate_with`].
#[must_use]
pub fn open_gate(path: &str) -> Option<reply::Refused> {
    let exe = std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned());
    open_gate_with(path, exe.as_deref(), Some(&command_line()), |host| {
        match platform::read_vpcf(host) {
            HostRead::Refused(why) => Err(why),
            HostRead::Bytes(bytes) => Ok(bytes),
        }
    })
}

/// The process's command line, for [`addon_name`]'s and `map.open`'s caller shape
/// (`ops/map.rs`'s `raw_process_facts` forwards it raw, like `open_gate` does).
pub(crate) fn command_line() -> String {
    std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}
