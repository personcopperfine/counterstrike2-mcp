//! The Hammer core and the read-only Hammer document ops: `doc.active`, `node.list`,
//! `entity.get`, `entity.find` (M4 lane HAMMER-R).
//!
//! This module is the C bridge's Hammer half below the writes:
//!
//! * the object chain - `resolve_hammer` / `active_doc` with every identity hop
//!   (`mcp_bridge.c:1693-1735`), the document fingerprint (`:1736-1779`),
//!   `doc_editable` (`:1780-1802`) and the undo-history readers (`:1810-1829`);
//! * the whole `g_hfn` bind table (`:1642-1691`, signatures from `s2sdk.h:698-742`),
//!   bound once from the generated `HFN_*` pins, including the entries no read uses yet (the
//!   write lanes build on them);
//! * the DataModel/node helpers the reads use (`:1938-1989`, `:3119-3442`): `data_model`,
//!   `dm_intern`, `probe_str`, `attr_cstr`, `elem_attr`, `node_id_of`, `node_origin` /
//!   `node_angles` / `node_scales`, `node_class`, `geo_walk` / `geo_world` / `geo_find`,
//!   `entity_value_json`, the live connection and child lists;
//! * the four read ops (`:3443-3713`, `:4565-4670`, `:5512-5538`) and `doc.active`
//!   (`:6234-6277`).
//!
//! # How it is split
//!
//! Everything that reads game memory is behind the [`Chain`] / [`Nodes`] / [`Hammer`] traits,
//! whose production implementation is [`win::LiveHammer`] (the Windows tools build only: the
//! generated `hammer` / `mapdoclib` / `toolutils2` modules are `cfg(windows)`). The JSON
//! builders and every parameter validation are plain functions over strings, numbers and the
//! small structs the reader fills ([`DocFacts`], [`EntityRead`], [`ChildEntry`]), so the host
//! tests hold them to the C's `sb_add` sequences byte for byte.
//!
//! On a non-Windows target there is no Hammer module at all: [`game`] answers [`NoHammer`],
//! whose [`Hammer::fp_sample`] is all-null (the tick's fingerprint is a no-op) and whose
//! [`Hammer::active_doc`] answers the C's `hammer_not_loaded` reason. No node read can run
//! there because the method gate (`-32017`) never lets a body start; the [`Nodes`] methods
//! fail loudly instead of guessing.
//!
//! # Statics
//!
//! The C keeps `g_hfn`, `g_hammer`, `g_fp` and the handle table in process-wide statics, and
//! all of them are touched on the tools main thread only: the bind happens inside the first
//! `resolve_hammer` (a node read or the tick's fingerprint) and every read body runs in the
//! tick's drain. The Rust port keeps the same shape with [`Mutex`]es; no other thread calls
//! into this module.

#[cfg(any(windows, test))]
use core::ffi::c_void;
use std::ffi::CStr;
use std::sync::{Mutex, MutexGuard};

use crate::control;
use crate::entity_json::{
    json_find_top, json_float1_top, json_float3_top, json_int, json_int_top, json_string,
    json_string_top, utf8_prefix, xform_any, xform_origin_ok, xform_origin_reason,
    xform_scales_ok, ConnSpec, Sb,
};
use crate::generated::pins;
use crate::log;
use crate::ops::{finish, OpErr};
use crate::queue::Request;

/// `GEO_MAX_NODES` (`mcp_bridge.c:3119`): the walk's global visit budget.
pub(crate) const GEO_MAX_NODES: u32 = 200_000;

/// The walk depth cap (`mcp_bridge.c:3160`, `:4570`).
pub(crate) const GEO_MAX_DEPTH: i32 = 64;

/// `NODEID_MAX` (`mcp_bridge.c:3440`): node ids are an i32 on the wire.
pub(crate) const NODEID_MAX: i64 = 2_147_483_647;

/// `ENTITY_MAX_ATTRS` (`mcp_bridge.c:3445`).
#[allow(dead_code)] // the Windows reader and the host tests are the callers
const ENTITY_MAX_ATTRS: usize = 512;

/// `ENTITY_STR_CAP` (`mcp_bridge.c:3446`).
#[allow(dead_code)]
const ENTITY_STR_CAP: usize = 4096;

/// `ENTITY_FIND_MAX` (`mcp_bridge.c:3447`).
const ENTITY_FIND_MAX: i64 = 2000;

/// `ENTITY_FIND_DEFAULT` (`mcp_bridge.c:3448`).
const ENTITY_FIND_DEFAULT: i64 = 500;

/// `CONN_VEC_SANE` (`mcp_bridge.c:3553`).
#[allow(dead_code)]
const CONN_VEC_SANE: i32 = 65_535;

/// `RM_INDEX_MAX` (`mcp_bridge.c:3527`): the connection output cap, also `entity_json`'s.
#[allow(dead_code)]
const RM_INDEX_MAX: usize = 512;

/// `CHILDREN_LIST_MAX` (`mcp_bridge.c:3648`).
pub(crate) const CHILDREN_LIST_MAX: usize = 1024;

/// `PROBE_PAGE_MASK` (`mcp_bridge.c:1959`): re-probe readability every 4 KB.
#[allow(dead_code)]
const PROBE_PAGE_MASK: usize = 4095;

/// The C's op result buffer: `static char rbuf[512 * 1024]` in `geo_dispatch`
/// (`mcp_bridge.c:5747`).
pub(crate) const OP_RESULT_CAP: usize = 512 * 1024;

/// The C's `S2_ATTR_TYPE_MASK` and the four DME types the bridge reads and writes
/// (`s2sdk.h:316-321`). These are bridge literals read out of hammer.dll - no upstream
/// enum, so no generated name exists.
#[allow(dead_code)] // the Windows writer and the host tests' fake use it; a read-only host build does not
const ATTR_TYPE_MASK: u32 = 0x3F;
/// `S2_AT_INT`.
const AT_AT_INT: u32 = 2;
/// `S2_AT_FLOAT`.
const AT_AT_FLOAT: u32 = 3;
/// `S2_AT_BOOL`.
const AT_AT_BOOL: u32 = 4;
/// `S2_AT_STRING`.
const AT_AT_STRING: u32 = 5;

/// A document: the `CMapDoc *` the chain resolved, as an opaque token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Doc(pub usize);

/// A node: the `CMapNode *` a walk reached, as an opaque token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Node(pub usize);

// ---------------------------------------------------------------------------------------------
// C string helpers (`seq`, `ci_seq`, `mcp_bridge.c:157`, `:173`)
// ---------------------------------------------------------------------------------------------

/// The C string view of `bytes`: everything before the first NUL.
pub(crate) fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&byte| byte == 0) {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

/// `seq` (`mcp_bridge.c:157`): byte equality of two C strings, stop at the first NUL.
pub(crate) fn seq(a: &[u8], b: &[u8]) -> bool {
    cstr(a) == cstr(b)
}

/// `ci_seq` (`mcp_bridge.c:173-180`): ASCII-case-insensitive `seq`.
pub(crate) fn ci_seq(a: &[u8], b: &[u8]) -> bool {
    cstr(a).eq_ignore_ascii_case(cstr(b))
}

/// `seq`'s early exit against a game-owned C string: is `text` exactly `name`?
///
/// The C's `seq(a->name, name)` (`mcp_bridge.c:3288`) walks both strings and returns at the
/// first difference or the shared NUL; it never measures `text`, so the comparison reads at
/// most `name.len() + 1` bytes of game memory. That is what this does - never
/// `CStr::from_ptr`, which walks the whole string first.
///
/// # Safety
///
/// `text` is a game pointer whose first byte the caller proved readable with [`mem_ok`]; every
/// further byte is re-probed here, so an unreadable name answers `false` instead of faulting
/// (the C reads unguarded past byte 0; this cannot change a reply for a readable string).
#[allow(dead_code)] // the Windows reader and the host tests are the callers
unsafe fn name_is(text: *const u8, name: &core::ffi::CStr) -> bool {
    for (i, want) in name.to_bytes_with_nul().iter().enumerate() {
        let at = text.wrapping_add(i);
        if !mem_ok(at.cast()) {
            return false;
        }
        // SAFETY: `mem_ok` just proved this byte readable.
        if unsafe { at.read() } != *want {
            return false;
        }
    }
    true
}

/// A game byte read as a flag: `*at != 0`, never a Rust `bool`.
///
/// The C reads `doc->m_bActive` (`mcp_bridge.c:6246`) and `session->m_bIsNew` (`:6248`) as a
/// `u32` out of a C++ `bool` field. Forming a Rust `bool` from a game byte other than 0/1 is
/// UB, so the readers hand the field's address here and only the truthiness crosses.
///
/// # Safety
///
/// `at` points at the one-byte field the C reads (`m_bActive` / `m_bIsNew`), inside an object
/// the caller identity-checked.
#[allow(dead_code)] // the Windows reader and the host tests are the callers
unsafe fn flag_byte(at: *const u8) -> bool {
    // SAFETY: the caller proved the object and the field's address belong to the game object.
    unsafe { at.read() != 0 }
}

/// `probe_str` (`mcp_bridge.c:1971-1985`): copy the game-owned C string `s` into `out`.
///
/// Returns 1 when the whole string fit, -1 when it was cut at `cap - 1` (a valid UTF-8 prefix
/// is left in `out`), 0 when `s` is null or unreadable (or `cap < 2`, which writes `out[0]`).
/// Readability is re-probed at every 4 KB page boundary, as the C does.
///
/// # Safety
///
/// `s` is a game pointer: it is only ever *read* through [`mem_ok`], which reports an
/// unreadable address instead of faulting on both targets (the C's `ReadProcessMemory`; the
/// host's `process_vm_readv`).
#[allow(dead_code)] // the Windows reader and the host tests are the callers
pub(crate) unsafe fn probe_str(s: *const u8, out: &mut [u8]) -> i32 {
    let cap = out.len();
    if cap == 0 {
        return 0;
    }
    out[0] = 0;
    if s.is_null() || !mem_ok(s.cast()) || cap < 2 {
        return 0;
    }
    let mut i = 0usize;
    while i + 1 < cap {
        if i != 0 && ((s as usize).wrapping_add(i) & PROBE_PAGE_MASK) == 0 && !mem_ok(s.wrapping_add(i).cast()) {
            out[0] = 0;
            return 0;
        }
        // SAFETY: the first byte was proven readable, and every page boundary of the walk is
        // re-probed above; the C reads the same bytes (`:1980`).
        let byte = unsafe { s.add(i).read() };
        out[i] = byte;
        if byte == 0 {
            return 1;
        }
        i += 1;
    }
    // The copy filled cap-1 bytes without a NUL: the next byte decides whether the string is
    // exactly cap-1 bytes (it fits) or longer (a cut).
    // SAFETY: `mem_ok` proves the byte readable before it is read, exactly as the C does.
    if unsafe { mem_ok(s.add(cap - 1).cast()) && s.add(cap - 1).read() == 0 } {
        out[cap - 1] = 0;
        return 1;
    }
    let n = utf8_prefix(out, (cap - 1) as u32) as usize;
    out[n] = 0;
    -1
}

/// `mem_ok` (`mcp_bridge.c:1182`), re-exported from [`crate::ops::ident`] for this module's
/// reader.
pub(crate) use crate::ops::ident::mem_ok;

// ---------------------------------------------------------------------------------------------
// SB helpers the C region uses for the reads
// ---------------------------------------------------------------------------------------------

/// `sb_addf3` (`mcp_bridge.c:2648-2653`): `[x,y,z]`.
pub(crate) fn addf3(b: &mut Sb, v: [f32; 3]) {
    b.add(b"[");
    b.addf(v[0]);
    b.add(b",");
    b.addf(v[1]);
    b.add(b",");
    b.addf(v[2]);
    b.add(b"]");
}

/// A JSON string value built from arbitrary bytes ([`Sb::addjson`] over a scratch builder).
#[allow(dead_code)]
fn json_str_bytes(bytes: &[u8]) -> String {
    let mut buf = vec![0u8; bytes.len().saturating_mul(6).saturating_add(3)];
    let mut sb = Sb::new(&mut buf);
    sb.addjson(bytes);
    String::from_utf8_lossy(sb.as_cstr()).into_owned()
}

// ---------------------------------------------------------------------------------------------
// Undo history info (`mcp_bridge.c:1810-1829`)
// ---------------------------------------------------------------------------------------------

/// One undo group's top entry: [`UndoGroup_t`]'s serial and label, as the reader sees them.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct UndoGroupView<'a> {
    /// `m_nSerial`.
    pub serial: i32,
    /// `m_szName` (128 bytes).
    pub name: &'a [u8],
}

/// `history_top_info`'s output: the history's group count and its top group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HistoryInfo {
    /// The label's bytes, cut at `namesz - 1` / 128 bytes / the first NUL, as the C copies
    /// them. Raw bytes, not text: [`Sb::addjson`] does the C's invalid-UTF-8 replacement.
    pub name: Vec<u8>,
    /// The history's own group count (`CMapDocHistory_Count`).
    pub depth: u32,
    /// The top group's serial (0 with no group).
    pub serial: u32,
}

/// `history_top_info` (`mcp_bridge.c:1810-1824`) over its two reads: the history's group count
/// and its top entry (`Groups(count - 1)`), already validated by the caller.
///
/// The C copies at most `namesz - 1` AND `sizeof m_szName - 1` bytes of the label
/// (`:1819-1821`); both the callers pass `namesz = 128`, the field's size.
#[allow(dead_code)] // the Windows reader's doc.active and the host tests
pub(crate) fn history_top_info(
    count: u32,
    top: Option<UndoGroupView<'_>>,
    namesz: u32,
) -> HistoryInfo {
    let mut info = HistoryInfo {
        name: Vec::new(),
        depth: count,
        serial: 0,
    };
    if count == 0 {
        return info;
    }
    let Some(entry) = top else {
        return info;
    };
    info.serial = entry.serial as u32;
    // while (i + 1 < namesz && i < sizeof entry->m_szName && entry->m_szName[i])
    let cap = (namesz as usize)
        .saturating_sub(1)
        .min(entry.name.len());
    let n = entry.name[..cap]
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(cap);
    // The C copies the label's bytes verbatim (`:1820-1821`); the lossy text conversion is
    // `sb_addjson`'s job, not the reader's (it replaces invalid UTF-8 the C's way).
    info.name = entry.name[..n].to_vec();
    info
}

// ---------------------------------------------------------------------------------------------
// The object chain (`mcp_bridge.c:1693-1802`)
// ---------------------------------------------------------------------------------------------

/// The identity/session hops `resolve_hammer` and `active_doc` take, as the reader sees them.
///
/// Every method is one unguarded read on an object the previous hop already identity-checked
/// (`obj_is`) or one identity check itself, exactly as the C's pointer chain does it
/// (`mcp_bridge.c:1693-1735`). A fake implementation drives the reason chain in the host tests.
pub(crate) trait Chain {
    /// `mod_ok(PIN_HAMMER)`'s refusal reason: `None` when verified, else the C's
    /// `hammer_mismatch` / `hammer_verifying` / `hammer_not_loaded`.
    fn module_reason(&self) -> Option<&'static str>;
    /// `resolve_hammer()`: the `CHammerApp *`, or `None` after the hop's own log line.
    fn app(&self) -> Option<usize>;
    /// `CHammerApp_ActiveSession` (`s2sdk.h:340`): may be null.
    fn session(&self, app: usize) -> Option<usize>;
    /// `session->m_pDocSession`.
    fn doc_session(&self, session: usize) -> Option<usize>;
    /// `CMapDocSession_Owner` (`s2sdk.h:392`).
    fn owner(&self, doc_session: usize) -> usize;
    /// `CMapDocSession_Doc` (`s2sdk.h:388`): the embedded document's address.
    fn doc_of(&self, doc_session: usize) -> usize;
    /// `doc->m_pDocSession`.
    fn doc_backptr(&self, doc: usize) -> usize;
    /// `obj_is(obj, PIN_HAMMER, vt)`.
    fn is_of(&self, object: usize, vtable: u32) -> bool;
    /// `doc->m_pRoot`.
    fn root(&self, doc: usize) -> usize;
    /// `doc->m_bActive`.
    fn active_flag(&self, doc: usize) -> bool;
    /// `doc->m_pSelection`.
    fn selection(&self, doc: usize) -> usize;
    /// `CMapDoc_ScopeDepth` (`s2sdk.h:384`).
    fn scope_depth(&self, doc: usize) -> u32;
    /// `g_hfn.GetUndoHistory(doc)`.
    fn undo_history(&self, doc: usize) -> usize;
    /// `*(CMapDocHistory **)(hb + HG_ACTIVE_HISTORY)` (`:1798`).
    fn active_history(&self) -> usize;
}

/// `active_doc` (`mcp_bridge.c:1713-1735`): the live document, or the C's short reason.
pub(crate) fn active_doc_with(chain: &dyn Chain) -> Result<Doc, &'static str> {
    if let Some(reason) = chain.module_reason() {
        return Err(reason);
    }
    let Some(app) = chain.app() else {
        return Err("hammer_unavailable");
    };
    let Some(session) = chain.session(app) else {
        return Err("no_session");
    };
    if !chain.is_of(session, pins::HV_SESSION_VT.value as u32) {
        return Err("bad_session");
    }
    let Some(doc_session) = chain.doc_session(session) else {
        return Err("no_document");
    };
    if !chain.is_of(doc_session, pins::HV_DOCSESSION_VT.value as u32) {
        return Err("bad_doc_session");
    }
    if chain.owner(doc_session) != session {
        return Err("doc_session_backptr");
    }
    let doc = chain.doc_of(doc_session);
    if !chain.is_of(doc, pins::HV_DOC_VT.value as u32) {
        return Err("bad_doc");
    }
    if chain.doc_backptr(doc) != doc_session {
        return Err("doc_backptr");
    }
    Ok(Doc(doc))
}

/// `doc_editable` (`mcp_bridge.c:1780-1802`): the full edit precondition set. `doc.active`
/// only reports it; the write lanes call it before their first mutation.
#[allow(dead_code)] // reached through the Windows reader and the host reason-chain test
pub(crate) fn doc_editable_with(chain: &dyn Chain, doc: Doc) -> (bool, &'static str) {
    if chain.root(doc.0) == 0 {
        return (false, "no_root");
    }
    if !chain.is_of(chain.root(doc.0), pins::HV_ROOT_VT.value as u32) {
        return (false, "bad_root");
    }
    if !chain.active_flag(doc.0) {
        return (false, "inactive");
    }
    if chain.selection(doc.0) == 0 {
        return (false, "no_selection");
    }
    if !chain.is_of(chain.selection(doc.0), pins::HV_SELECTION_VT.value as u32) {
        return (false, "bad_selection");
    }
    if chain.scope_depth(doc.0) != 0 {
        return (false, "edit_scope_active");
    }
    let hist = chain.undo_history(doc.0);
    if hist == 0 {
        return (false, "no_history");
    }
    if !chain.is_of(hist, pins::HV_HISTORY_VT.value as u32) {
        return (false, "bad_history");
    }
    if chain.active_history() != hist {
        return (false, "history_not_active");
    }
    (true, "ok")
}

// ---------------------------------------------------------------------------------------------
// The document fingerprint (`mcp_bridge.c:1736-1779`)
// ---------------------------------------------------------------------------------------------

/// The C's `DocFp` (`mcp_bridge.c:1736-1743`). Pointers are kept as `usize` so the state is
/// `Send`; the C compares the pointer values, and so does this.
#[derive(Clone, Copy, Debug, Default)]
struct DocFp {
    doc_session: usize,
    root: usize,
    path_ptr: usize,
    root_handle: u32,
    doc_seq: u32,
    epoch: u32,
}

/// One fingerprint sample: the four structural facts `doc_fingerprint` compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FpSample {
    /// `session->m_pDocSession`.
    pub doc_session: usize,
    /// `CMapDocSession_Doc(d)->m_pRoot`.
    pub root: usize,
    /// `CMapDocSession_Path(d)`: the path string's address.
    pub path_ptr: usize,
    /// `root->m_hWorld`.
    pub root_handle: u32,
}

/// The process-wide fingerprint state: C `g_fp` (`mcp_bridge.c:1744`).
static FP: Mutex<DocFp> = Mutex::new(DocFp {
    doc_session: 0,
    root: 0,
    path_ptr: 0,
    root_handle: 0,
    doc_seq: 0,
    epoch: 0,
});

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One `doc_fingerprint` step over an explicit state (`mcp_bridge.c:1746-1775`): `true` when
/// the sample changed the fingerprint (the C logs the new epoch).
fn fingerprint_step(fp: &mut DocFp, sample: FpSample) -> bool {
    if sample.doc_session == fp.doc_session
        && sample.root == fp.root
        && sample.path_ptr == fp.path_ptr
        && sample.root_handle == fp.root_handle
    {
        return false;
    }
    if sample.doc_session != fp.doc_session {
        fp.doc_seq = fp.doc_seq.wrapping_add(1);
    }
    fp.doc_session = sample.doc_session;
    fp.root = sample.root;
    fp.path_ptr = sample.path_ptr;
    fp.root_handle = sample.root_handle;
    fp.epoch = fp.epoch.wrapping_add(1);
    true
}

/// `doc_fingerprint` (`mcp_bridge.c:1746-1775`), sampled on every tick (`m0_Slot13`, the C at
/// `:6819`) and at the head of every reply that reports the document id. Main thread only.
pub fn doc_fingerprint() {
    let sample = game().fp_sample();
    let mut fp = lock(&FP);
    if fingerprint_step(&mut fp, sample) {
        log::line(&format!("doc: fingerprint change, epoch {}", fp.epoch)); // :1773-1774
    }
}

/// The current `(docSeq, epoch)` - the id/epoch every reply reports.
pub(crate) fn fingerprint_ids() -> (u32, u32) {
    let fp = lock(&FP);
    (fp.doc_seq, fp.epoch)
}

/// `sb_add_doc_object` (`mcp_bridge.c:1860-1871`): `{"id":"dN","topSerial":N,"undoDepth":N}`,
/// the state a write's own reply carries. The fingerprint is refreshed first (`:1861`).
pub(crate) fn sb_add_doc_object(b: &mut Sb, doc: Doc) {
    doc_fingerprint();
    let (seq, _) = fingerprint_ids();
    let info = game().undo_info(doc);
    b.add(b"{\"id\":\"d");
    b.addnum(u64::from(seq));
    b.add(b"\",\"topSerial\":");
    b.addnum(u64::from(info.serial));
    b.add(b",\"undoDepth\":");
    b.addnum(u64::from(info.depth));
    b.add(b"}");
}

// ---------------------------------------------------------------------------------------------
// `doc.active` (`mcp_bridge.c:6234-6277`)
// ---------------------------------------------------------------------------------------------

/// The fields `doc.active` reports, read from the live document by [`Hammer::doc_facts`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DocFacts {
    /// `CMapDocSession_Path(docSession)`, as the bytes the C hands to `sb_addjson`.
    pub path: Vec<u8>,
    /// `ChangedSinceSave(mgr, doc)`, `None` when the manager global is null (the C's `null`).
    pub dirty: Option<bool>,
    /// `doc->m_bActive`.
    pub active: bool,
    /// `doc_editable`'s verdict.
    pub editable: bool,
    /// `doc_editable`'s reason (`"ok"` when editable).
    pub why: &'static str,
    /// `session->m_bIsNew`.
    pub is_new: bool,
    /// `undo_group_info` of the document's undo stack.
    pub undo: HistoryInfo,
}

/// The `doc` object of `doc.active` (`mcp_bridge.c:6250-6272`), written into the reply builder
/// after `"result":{`.
fn doc_object_into(b: &mut Sb, doc_seq: u32, epoch: u32, facts: &DocFacts) {
    b.add(b"\"doc\":{\"id\":\"d");
    b.addnum(u64::from(doc_seq));
    b.add(b"\",\"path\":");
    b.addjson(&facts.path);
    b.add(b",\"dirty\":");
    match facts.dirty {
        None => b.add(b"null"),
        Some(true) => b.add(b"true"),
        Some(false) => b.add(b"false"),
    }
    b.add(b",\"active\":");
    b.add(if facts.active { b"true" } else { b"false" });
    b.add(b",\"editable\":");
    b.add(if facts.editable { b"true" } else { b"false" });
    b.add(b",\"notEditableReason\":");
    b.addjson(if facts.editable { b"ok" } else { facts.why.as_bytes() });
    b.add(b",\"epoch\":");
    b.addnum(u64::from(epoch));
    b.add(b",\"docType\":\"vmap\",\"isNewFile\":");
    b.add(if facts.is_new { b"true" } else { b"false" });
    b.add(b",\"undoName\":");
    if facts.undo.depth != 0 {
        b.addjson(&facts.undo.name);
    } else {
        b.add(b"null");
    }
    b.add(b",\"undoDepth\":");
    b.addnum(u64::from(facts.undo.depth));
    b.add(b",\"topSerial\":");
    b.addnum(u64::from(facts.undo.serial));
    b.add(b"}");
}

/// The whole `doc.active` reply (`mcp_bridge.c:6239-6275`), byte for byte: the envelope with
/// the `bridge` member, `result.doc` either `null` + `reason` or the document object.
fn doc_active_reply(id: &str, resolved: Option<(u32, u32, &DocFacts)>, reason: &str) -> String {
    let mut buf = vec![0u8; control::MAX_LINE as usize];
    let mut b = Sb::new(&mut buf);
    b.add(b"{\"jsonrpc\":\"2.0\",\"id\":");
    b.addjson(id.as_bytes());
    b.add(b",\"bridge\":{\"protocol\":1,\"build\":\"");
    b.add(crate::BRIDGE_BUILD.as_bytes());
    b.add(b"\"},\"result\":{");
    match resolved {
        None => {
            b.add(b"\"doc\":null,\"reason\":");
            b.addjson(reason.as_bytes());
        }
        Some((seq, epoch, facts)) => doc_object_into(&mut b, seq, epoch, facts),
    }
    b.add(b"}}\n");
    if b.ovf() {
        log::line("exec: response buffer overflow"); // :6276
    }
    String::from_utf8_lossy(b.as_cstr()).into_owned()
}

/// `doc.active`'s body: resolve the document, sample the fingerprint, answer.
fn doc_active(req: &Request) -> String {
    let hammer = game();
    let resolved = hammer.active_doc();
    doc_fingerprint(); // :6241: epoch/id current for this answer
    let (seq, epoch) = fingerprint_ids();
    match resolved {
        Err(reason) => doc_active_reply(&req.id, None, reason),
        Ok(doc) => {
            let facts = hammer.doc_facts(doc);
            doc_active_reply(&req.id, Some((seq, epoch, &facts)), "ok")
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The reader contract and the walk (`mcp_bridge.c:3119-3443`, `:4565-4637`)
// ---------------------------------------------------------------------------------------------

/// One `keyValues` value of an entity's properties element (`entity_value_json`,
/// `mcp_bridge.c:3455-3475`).
#[allow(dead_code)] // constructed by the Windows reader and by the host builder tests
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AttrValue {
    /// A `S2_AT_STRING` whose text fit (emitted as a JSON string).
    Str(Vec<u8>),
    /// `S2_AT_INT`.
    Int(i32),
    /// `S2_AT_FLOAT`.
    Float(f32),
    /// `S2_AT_BOOL`.
    Bool(bool),
    /// Any other type, an unreadable storage, or a null attribute: `null` on the wire.
    Null {
        /// The type is not one of the four `entity_type_supported` names: list the attribute in
        /// `unsupported`.
        unsupported: bool,
        /// A string value was cut: emit `null` and list the name in `truncatedValues`.
        cut: bool,
    },
}

/// One live connection entry (`S2ConnectionSrc`, `s2sdk.h:424-447`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Conn {
    /// `output`
    pub output: Vec<u8>,
    /// `targetType`
    pub target_type: i32,
    /// `target`
    pub target: Vec<u8>,
    /// `input`
    pub input: Vec<u8>,
    /// `param`
    pub param: Vec<u8>,
    /// `delay`
    pub delay: f32,
    /// `timesToFire`
    pub times_to_fire: i32,
    /// A string member was cut (`sb_add_conn_str` answered 1).
    pub cut: bool,
}

/// One `children[]` entry (`entity_children_json`, `mcp_bridge.c:3650-3690`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChildEntry {
    /// `null` when `node_id_of` failed (the C's review-b L6 rule).
    pub id: Option<i32>,
    /// `node_class`.
    pub class: &'static str,
}

/// The live connection list: `null` for a read failure, else the entries (and the real total
/// when the 512-entry output cap cut it).
#[allow(dead_code)] // constructed by the Windows reader and by the host builder tests
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConnList {
    /// `null`.
    Null,
    /// `[...]`, with `Some(total)` when the cap cut the list.
    List { entries: Vec<Conn>, total: Option<i32> },
}

/// The one-level child list: `null` for a read failure, else the entries.
#[allow(dead_code)] // constructed by the Windows reader and by the host builder tests
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChildList {
    /// `null`.
    Null,
    /// `[...]`, with `Some(total)` when the 1024-entry cap cut the list.
    List {
        entries: Vec<ChildEntry>,
        total: Option<i32>,
    },
}

/// Everything `entity.get` prints, read in one pass by [`Hammer::entity_read`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EntityRead {
    /// The `classname` attribute (raw game bytes; `addjson` escapes them).
    pub classname: Vec<u8>,
    /// The `targetname` attribute (raw game bytes).
    pub targetname: Vec<u8>,
    /// `node_origin` (zeros when unreadable).
    pub origin: [f32; 3],
    /// `node_angles`, `None` when unreadable (the C's `null`).
    pub angles: Option<[f32; 3]>,
    /// `node_scales`, `None` when unreadable (the C's `null`).
    pub scales: Option<[f32; 3]>,
    /// The `keyValues` attributes in element order (`id`/`name` dropped), raw name bytes.
    pub attrs: Vec<(Vec<u8>, AttrValue)>,
    /// The attribute-walk cap (`ENTITY_MAX_ATTRS`) was hit.
    pub truncated: bool,
    /// The live I/O vector.
    pub connections: ConnList,
    /// The node's children, one level.
    pub children: ChildList,
}

/// The tree reads the walks need, one level at a time.
pub(crate) trait Nodes {
    /// `CMapNode`'s children as map nodes (`geo_walk`'s guard block, `mcp_bridge.c:3160-3166`).
    /// `None` is the C's early `return`: an unreadable/corrupt child vector. `Some(vec![])` is
    /// a node with no children.
    fn children(&self, node: Node) -> Option<Vec<Node>>;
    /// `node_class` (`:3297-3306`).
    fn class(&self, node: Node) -> &'static str;
    /// `node_class` is `entity` (the C's `obj_is(.., HV_MAPENTITY_VT)`).
    fn is_entity(&self, node: Node) -> bool {
        self.class(node) == "entity"
    }
    /// `node_id_of` (`:3281-3287`).
    fn id(&self, node: Node) -> Option<i32>;
    /// `node_origin` (`:3288-3293`).
    fn origin(&self, node: Node) -> Option<[f32; 3]>;
    /// `CMapMesh_PolyMesh(node)`'s two component counts (`:3207-3208`).
    fn mesh_slots(&self, node: Node) -> Option<(i32, i32)>;
    /// `geo_world` (`:3212-3215`).
    fn world(&self, doc: Doc) -> Option<Node>;
}

/// The whole live Hammer state the four read ops need.
pub(crate) trait Hammer: Chain + Nodes {
    /// `active_doc` (`:1713-1735`).
    fn active_doc(&self) -> Result<Doc, &'static str>;
    /// One fingerprint sample (`doc_fingerprint`'s reads, `:1748-1762`).
    fn fp_sample(&self) -> FpSample;
    /// `doc.active`'s fields (`:6244-6256`).
    fn doc_facts(&self, doc: Doc) -> DocFacts;
    /// `undo_group_info` (`:1827-1829`).
    fn undo_info(&self, doc: Doc) -> HistoryInfo;
    /// `GetEntityProperties` + the `keyValues` walk + the live connection/child lists
    /// (`:3694-3713`). `None` is the C's `entity_properties missing` (`-32603`).
    fn entity_read(&self, node: Node) -> Option<EntityRead>;
    /// The two strings and the origin `entity.find` filters on (`:4578-4596`), raw game bytes.
    fn entity_strings(&self, node: Node) -> (Vec<u8>, Vec<u8>, [f32; 3]);
}

/// The `geo_walk` list mode's state (`GWalk`, `mcp_bridge.c:3159`).
struct GeoWalk<'a> {
    want_id: Option<i32>,
    found: Option<Node>,
    visited: u32,
    listed: i32,
    limit: i32,
    class: Option<&'a [u8]>,
    truncated: bool,
}

/// `geo_walk` (`mcp_bridge.c:3160-3210`), list mode (`wantId < 0`).
fn geo_walk_list(nodes: &dyn Nodes, w: &mut GeoWalk<'_>, node: Node, depth: i32, out: &mut Sb) {
    if depth > GEO_MAX_DEPTH || w.visited > GEO_MAX_NODES || w.found.is_some() {
        return;
    }
    let Some(children) = nodes.children(node) else {
        return;
    };
    for child in children {
        if w.found.is_some() {
            break;
        }
        w.visited += 1;
        let id = nodes.id(child);
        let class = nodes.class(child);
        let matches = w.class.is_none_or(|filter| {
            seq(filter, b"all") || seq(filter, class.as_bytes())
        });
        if matches {
            if w.listed >= w.limit {
                w.truncated = true;
            } else {
                let origin = nodes.origin(child).unwrap_or([0.0, 0.0, 0.0]);
                if w.listed != 0 {
                    out.add(b",");
                }
                out.add(b"{\"nodeID\":");
                out.addi(i64::from(id.unwrap_or(-1)));
                out.add(b",\"class\":\"");
                out.add(class.as_bytes());
                out.add(b"\",\"origin\":");
                addf3(out, origin);
                if class.as_bytes().starts_with(b"me") {
                    if let Some((verts, faces)) = nodes.mesh_slots(child) {
                        out.add(b",\"vertexSlots\":");
                        out.addi(i64::from(verts));
                        out.add(b",\"faceSlots\":");
                        out.addi(i64::from(faces));
                    }
                }
                out.add(b"}");
                w.listed += 1;
            }
        }
        geo_walk_list(nodes, w, child, depth + 1, out);
    }
}

/// `geo_find` (`mcp_bridge.c:3216-3224`): the node with `id` under the world.
pub(crate) fn geo_find(nodes: &dyn Nodes, doc: Doc, id: i32) -> Option<Node> {
    let world = nodes.world(doc)?;
    let mut w = GeoWalk {
        want_id: Some(id),
        found: None,
        visited: 0,
        listed: 0,
        limit: 0,
        class: None,
        truncated: false,
    };
    // The id search reuses the same child walk; the list branch is not taken.
    fn visit(nodes: &dyn Nodes, w: &mut GeoWalk<'_>, node: Node, depth: i32) {
        if depth > GEO_MAX_DEPTH || w.visited > GEO_MAX_NODES || w.found.is_some() {
            return;
        }
        let Some(children) = nodes.children(node) else {
            return;
        };
        for child in children {
            if w.found.is_some() {
                return;
            }
            w.visited += 1;
            if w.want_id.is_some() && nodes.id(child) == w.want_id {
                w.found = Some(child);
                return;
            }
            visit(nodes, w, child, depth + 1);
        }
    }
    visit(nodes, &mut w, world, 0);
    w.found
}

/// `entity_find_walk` (`mcp_bridge.c:4567-4617`).
struct EntityWalk<'a> {
    class: Option<&'a [u8]>,
    targetname: Option<&'a [u8]>,
    has_within: bool,
    center: [f32; 3],
    radius2: f32,
    visited: u32,
    listed: i32,
    limit: i32,
    truncated: bool,
}

fn entity_find_walk(
    hammer: &dyn Hammer,
    w: &mut EntityWalk<'_>,
    node: Node,
    depth: i32,
    out: &mut Sb,
) {
    if depth > GEO_MAX_DEPTH || w.visited > GEO_MAX_NODES {
        return;
    }
    let Some(children) = hammer.children(node) else {
        return;
    };
    for child in children {
        w.visited += 1;
        if hammer.is_entity(child) {
            let (classname, targetname, origin) = hammer.entity_strings(child);
            let mut matched = w
                .class
                .is_none_or(|filter| ci_seq(filter, &classname))
                && w.targetname
                    .is_none_or(|filter| ci_seq(filter, &targetname));
            if matched && w.has_within {
                let dx = origin[0] - w.center[0];
                let dy = origin[1] - w.center[1];
                let dz = origin[2] - w.center[2];
                if dx * dx + dy * dy + dz * dz > w.radius2 {
                    matched = false;
                }
            }
            if matched {
                if w.listed >= w.limit {
                    w.truncated = true;
                } else {
                    let id = hammer.id(child).unwrap_or(-1);
                    if w.listed != 0 {
                        out.add(b",");
                    }
                    out.add(b"{\"nodeID\":");
                    out.addi(i64::from(id));
                    out.add(b",\"classname\":");
                    out.addjson(&classname);
                    out.add(b",\"targetname\":");
                    out.addjson(&targetname);
                    out.add(b",\"origin\":");
                    addf3(out, origin);
                    out.add(b"}");
                    w.listed += 1;
                }
            }
        }
        entity_find_walk(hammer, w, child, depth + 1, out);
    }
}

// ---------------------------------------------------------------------------------------------
// The `keyValues` / connection / child builders (`mcp_bridge.c:3455-3690`)
// ---------------------------------------------------------------------------------------------

/// One `entity.get` `keyValues` value (`entity_value_json`, `:3455-3475`).
fn attr_value_into(out: &mut Sb, value: &AttrValue) {
    match value {
        AttrValue::Str(text) => out.addjson(text),
        AttrValue::Int(value) => out.addi(i64::from(*value)),
        AttrValue::Float(value) => out.addf(*value),
        AttrValue::Bool(value) => out.add(if *value { b"true" } else { b"false" }),
        AttrValue::Null { .. } => out.add(b"null"),
    }
}

/// `sb_add_conn_fields` (`mcp_bridge.c:3593-3604`): one live connection.
fn conn_into(out: &mut Sb, conn: &Conn) {
    out.add(b"\"outputName\":");
    out.addjson(&conn.output);
    out.add(b",\"targetType\":");
    out.addi(i64::from(conn.target_type));
    out.add(b",\"targetName\":");
    out.addjson(&conn.target);
    out.add(b",\"inputName\":");
    out.addjson(&conn.input);
    out.add(b",\"overrideParam\":");
    out.addjson(&conn.param);
    out.add(b",\"delay\":");
    out.addf(conn.delay);
    out.add(b",\"timesToFire\":");
    out.addi(i64::from(conn.times_to_fire));
    if conn.cut {
        out.add(b",\"truncated\":true");
    }
}

/// `conn_list_json` (`mcp_bridge.c:3612-3632`). Returns the real total when the output cap cut
/// the list.
fn conn_list_into(out: &mut Sb, list: &ConnList) -> Option<i32> {
    match list {
        ConnList::Null => {
            out.add(b"null");
            None
        }
        ConnList::List { entries, total } => {
            out.add(b"[");
            for (i, conn) in entries.iter().enumerate() {
                if i != 0 {
                    out.add(b",");
                }
                out.add(b"{");
                conn_into(out, conn);
                out.add(b"}");
            }
            out.add(b"]");
            *total
        }
    }
}

/// `entity_children_json` (`mcp_bridge.c:3652-3691`). Returns the real total when the cap cut
/// the list.
fn children_into(out: &mut Sb, list: &ChildList) -> Option<i32> {
    match list {
        ChildList::Null => {
            out.add(b"null");
            None
        }
        ChildList::List { entries, total } => {
            out.add(b"[");
            for (i, child) in entries.iter().enumerate() {
                if i != 0 {
                    out.add(b",");
                }
                out.add(b"{\"nodeID\":");
                // `:3682`: `if (id >= 0) sb_addi(out, id); else sb_add(out, "null")` - an
                // unreadable OR negative id is null, never a node a reader could address.
                match child.id.filter(|&id| id >= 0) {
                    Some(id) => out.addi(i64::from(id)),
                    None => out.add(b"null"),
                }
                out.add(b",\"class\":\"");
                out.add(child.class.as_bytes());
                out.add(b"\"}");
            }
            out.add(b"]");
            *total
        }
    }
}

/// `op_entity_get`'s result object (`mcp_bridge.c:3715-3838`), byte for byte.
///
/// Returns `true` when either name list overflowed its own 1024-byte builder (`ubuf` / `vbuf`,
/// `:3752-3753`); the C turns that into `out->ovf` (`:3834`), so the arm answers
/// `-32603 "entity has too many or too large keyvalues"` instead of emitting cut JSON.
fn entity_get_into(out: &mut Sb, id: i64, read: &EntityRead) -> bool {
    out.add(b"{\"nodeID\":");
    out.addi(id);
    out.add(b",\"classname\":");
    out.addjson(&read.classname);
    out.add(b",\"targetname\":");
    out.addjson(&read.targetname);
    out.add(b",\"origin\":");
    addf3(out, read.origin);
    out.add(b",\"angles\":");
    match read.angles {
        Some(angles) => addf3(out, angles),
        None => out.add(b"null"),
    }
    out.add(b",\"scales\":");
    match read.scales {
        Some(scales) => addf3(out, scales),
        None => out.add(b"null"),
    }
    out.add(b",\"keyValues\":{");

    // `:3752-3755`: `static char ubuf[1024], vbuf[1024]` hold the two name lists.
    let mut ubuf = vec![0u8; 1024];
    let mut vbuf = vec![0u8; 1024];
    let mut us = Sb::new(&mut ubuf);
    let mut vs = Sb::new(&mut vbuf);
    // `id` is synthetic (the node's own id); the attribute walk's order is the element's.
    let mut unsup = 0usize;
    let mut vfirst = true;
    let mut first = true;
    for (name, value) in &read.attrs {
        if !first {
            out.add(b",");
        }
        first = false;
        out.addjson(name);
        out.add(b":");
        if let AttrValue::Null {
            unsupported: is_unsupported,
            cut,
        } = value
        {
            // `:3783-3787`: at most 64 unsupported names, each escaped into `ubuf`.
            if *is_unsupported && unsup < 64 {
                if unsup != 0 {
                    us.add(b",");
                }
                us.addjson(name);
                unsup += 1;
            }
            // `:3789`: every cut value's name, escaped into `vbuf`.
            if *cut {
                if !vfirst {
                    vs.add(b",");
                }
                vfirst = false;
                vs.addjson(name);
            }
        }
        attr_value_into(out, value);
    }
    out.add(b"},\"keyValueCount\":");
    out.addi(read.attrs.len() as i64);
    out.add(b",\"unsupported\":[");
    out.add(us.as_cstr());
    out.add(b"],\"truncatedValues\":[");
    out.add(vs.as_cstr());
    out.add(b"]");
    if read.truncated {
        out.add(b",\"truncated\":true");
    }
    out.add(b",\"connections\":");
    let conn_total = conn_list_into(out, &read.connections);
    if let Some(total) = conn_total {
        out.add(b",\"connectionsTruncated\":true,\"connectionsTotal\":");
        out.addi(i64::from(total));
    }
    out.add(b",\"children\":");
    let children_total = children_into(out, &read.children);
    if let Some(total) = children_total {
        out.add(b",\"childrenTruncated\":true,\"childrenTotal\":");
        out.addi(i64::from(total));
    }
    out.add(b",\"source\":\"live\"}");
    // `:3834`: `if (us.ovf || vs.ovf) out->ovf = 1` - either list failing fails the whole op.
    us.ovf() || vs.ovf()
}

// ---------------------------------------------------------------------------------------------
// The op bodies (`mcp_bridge.c:3715-3838`, `:4638-4668`, `:5512-5538`)
// ---------------------------------------------------------------------------------------------

/// `geo_entity_node` (`mcp_bridge.c:3443-3456`): a checked entity node by id.
pub(crate) fn geo_entity_node(nodes: &dyn Nodes, doc: Doc, id: i64) -> Result<Node, OpErr> {
    if !(0..=NODEID_MAX).contains(&id) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    }
    let Some(node) = geo_find(nodes, doc, id as i32) else {
        return Err(OpErr::new(
            -32022,
            "NodeNotFound",
            "no node with this nodeID in the live document",
        ));
    };
    if !nodes.is_entity(node) {
        return Err(OpErr::new(-32602, "InvalidParams", "not an entity"));
    }
    Ok(node)
}

/// `op_entity_get` (`mcp_bridge.c:3694-3713` + `:3839`): validate, read, build.
fn op_entity_get(
    hammer: &dyn Hammer,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    };
    if json_find_top(params.as_bytes(), b"nodeID").is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    }
    let Some(id) = json_int_top(params.as_bytes(), b"nodeID") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    let node = geo_entity_node(hammer, doc, id)?;
    let Some(read) = hammer.entity_read(node) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "entity_properties missing",
        ));
    };
    if entity_get_into(out, id, &read) {
        // `:3834` sets `out->ovf`; `geo_dispatch` answers the entity.get overflow message
        // (`:5774-5775`). The bare error is that same frame through `finish`.
        return Err(OpErr::bare(
            -32603,
            "entity has too many or too large keyvalues",
        ));
    }
    Ok(())
}

/// `op_node_list` (`mcp_bridge.c:5512-5538`).
fn op_node_list(
    hammer: &dyn Hammer,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let mut class = [0u8; 16];
    let mut limit = 2000i64;
    if let Some(params) = params {
        json_string(params.as_bytes(), b"class", &mut class);
        if let Some(value) = json_int(params.as_bytes(), b"limit") {
            limit = value;
        }
    }
    if !(1..=10000).contains(&limit) {
        limit = 2000;
    }
    let Some(world) = hammer.world(doc) else {
        return Err(OpErr::new(-32603, "InternalError", "no world node"));
    };
    let mut walk = GeoWalk {
        want_id: None,
        found: None,
        visited: 0,
        listed: 0,
        limit: limit as i32,
        class: if class[0] != 0 {
            Some(cstr(&class))
        } else {
            None
        },
        truncated: false,
    };
    out.add(b"{\"nodes\":[");
    geo_walk_list(hammer, &mut walk, world, 0, out);
    out.add(b"],\"count\":");
    out.addi(i64::from(walk.listed));
    out.add(b",\"visited\":");
    out.addi(i64::from(walk.visited));
    out.add(b",\"truncated\":");
    out.add(if walk.truncated { b"true" } else { b"false" });
    out.add(b",\"source\":\"live\"}");
    Ok(())
}

/// `op_entity_find` (`mcp_bridge.c:4638-4668`).
fn op_entity_find(
    hammer: &dyn Hammer,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let mut class = [0u8; 512];
    let mut targetname = [0u8; 512];
    let mut limit = ENTITY_FIND_DEFAULT;
    if let Some(params) = params {
        let bytes = params.as_bytes();
        json_string_top(bytes, b"class", &mut class);
        json_string_top(bytes, b"targetname", &mut targetname);
        // An empty filter is not a match-all: it is the caller's mistake.
        if json_find_top(bytes, b"class").is_some() && class[0] == 0 {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "class: a non-empty string is required",
            ));
        }
        if json_find_top(bytes, b"targetname").is_some() && targetname[0] == 0 {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "targetname: a non-empty string is required",
            ));
        }
        if json_find_top(bytes, b"limit").is_some() {
            let parsed = json_int_top(bytes, b"limit");
            match parsed {
                Some(value) if (1..=ENTITY_FIND_MAX).contains(&value) => limit = value,
                _ => {
                    return Err(OpErr::new(
                        -32602,
                        "InvalidParams",
                        "limit: an int in 1..2000 is required",
                    ))
                }
            }
        }
    }
    let mut center = [0.0f32; 3];
    let mut radius = 0.0f32;
    let mut has_within = false;
    if let Some(params) = params {
        let bytes = params.as_bytes();
        if let Some(start) = json_find_top(bytes, b"within") {
            let within = &bytes[start..];
            let origin = if within.first() == Some(&b'{') {
                json_float3_top(within, b"origin")
            } else {
                None
            };
            let parsed_radius = if within.first() == Some(&b'{') {
                json_float1_top(within, b"radius")
            } else {
                None
            };
            match (origin, parsed_radius) {
                (Some(origin), Some(parsed_radius)) if parsed_radius > 0.0 => {
                    center = origin;
                    radius = parsed_radius;
                    has_within = true;
                }
                _ => {
                    return Err(OpErr::new(
                        -32602,
                        "InvalidParams",
                        "within: {origin:[x,y,z], radius > 0} required",
                    ))
                }
            }
        }
    }
    let Some(world) = hammer.world(doc) else {
        return Err(OpErr::new(-32603, "InternalError", "no world node"));
    };
    let mut walk = EntityWalk {
        class: if class[0] != 0 {
            Some(cstr(&class))
        } else {
            None
        },
        targetname: if targetname[0] != 0 {
            Some(cstr(&targetname))
        } else {
            None
        },
        has_within,
        center,
        radius2: radius * radius,
        visited: 0,
        listed: 0,
        limit: limit as i32,
        truncated: false,
    };
    out.add(b"{\"entities\":[");
    entity_find_walk(hammer, &mut walk, world, 0, out);
    out.add(b"],\"count\":");
    out.addi(i64::from(walk.listed));
    out.add(b",\"visited\":");
    out.addi(i64::from(walk.visited));
    out.add(b",\"truncated\":");
    out.add(if walk.truncated { b"true" } else { b"false" });
    out.add(b",\"source\":\"live\"}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The write path: handles, `expectDoc`, the doc tail (`mcp_bridge.c:1832-1937`)
// ---------------------------------------------------------------------------------------------

/// `HANDLE_CAP` (`mcp_bridge.c:1886`): the token table's size.
const HANDLE_CAP: usize = 32;

/// The C's key-name / number-token buffers: `char name[96]` and `char nval[96]` in
/// `apply_keyvalues` (`mcp_bridge.c:2306`) and `op_entity_remove_connections` (`:3882`).
const KV_NAME_CAP: usize = 96;

/// One `Handle` (`mcp_bridge.c:1887`).
#[derive(Clone, Copy)]
struct Handle {
    used: bool,
    doc_seq: u32,
    epoch: u32,
    node: usize,
    token: [u8; 16],
}

/// C `g_handles` + `g_handleNext` (`mcp_bridge.c:1888-1889`): the tokens `entity.create` hands out
/// and `entity.setKeyValues` resolves. Main thread only, like every write body.
struct HandleTable {
    entries: [Handle; HANDLE_CAP],
    next: u32,
}

static HANDLES: Mutex<HandleTable> = Mutex::new(HandleTable {
    entries: [
        Handle {
            used: false,
            doc_seq: 0,
            epoch: 0,
            node: 0,
            token: [0; 16],
        };
        HANDLE_CAP
    ],
    next: 1,
});

/// `handle_new` (`mcp_bridge.c:1893-1913`): register `node` and answer its token.
///
/// A full table reuses `g_handleNext % HANDLE_CAP`, exactly as the C does, and the token is `n`
/// followed by `g_handleNext++` in decimal (at most 14 digits, so it always fits the 16-byte
/// field). The entry records the current fingerprint so [`handle_lookup`] can answer
/// `epoch_changed` after the document moved.
pub(crate) fn handle_new(node: usize) -> String {
    let (doc_seq, epoch) = fingerprint_ids();
    let mut table = lock(&HANDLES);
    let mut slot = HANDLE_CAP;
    for (index, entry) in table.entries.iter().enumerate() {
        if !entry.used {
            slot = index;
            break;
        }
    }
    if slot == HANDLE_CAP {
        slot = (table.next as usize) % HANDLE_CAP; // :1895
    }
    let mut token = [0u8; 16];
    token[0] = b'n'; // :1900
    let mut value = table.next;
    table.next = table.next.wrapping_add(1);
    if value == 0 {
        value = 1; // :1902
    }
    let mut digits = [0u8; 12];
    let mut count = 0;
    while value != 0 {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
    }
    let mut at = 1;
    while count != 0 && at + 1 < token.len() {
        count -= 1;
        token[at] = digits[count];
        at += 1;
    }
    table.entries[slot] = Handle {
        used: true,
        doc_seq,
        epoch,
        node,
        token,
    };
    String::from_utf8_lossy(cstr(&token)).into_owned()
}

/// `handle_lookup` (`mcp_bridge.c:1915-1926`): the node behind a token, or the C's `why`.
pub(crate) fn handle_lookup(token: &[u8]) -> Result<Node, &'static str> {
    let table = lock(&HANDLES);
    let (doc_seq, epoch) = fingerprint_ids();
    for entry in &table.entries {
        if !entry.used || !seq(&entry.token, token) {
            continue;
        }
        if entry.doc_seq != doc_seq || entry.epoch != epoch {
            return Err("epoch_changed");
        }
        return Ok(Node(entry.node));
    }
    Err("unknown_handle")
}

/// `expect_doc_ok` (`mcp_bridge.c:1928-1948`): the caller's `expectDoc {id, epoch}` is the current
/// fingerprint. `Ok(())` when it matches, else the C's `why` (`missing`, `missing_id`,
/// `missing_epoch`, `doc_id`, `epoch`).
pub(crate) fn expect_doc_ok(
    params: Option<&[u8]>,
    doc_seq: u32,
    epoch: u32,
) -> Result<(), &'static str> {
    let Some(params) = params else {
        return Err("missing"); // :1930
    };
    let Some(ed) = crate::entity_json::json_find_top(params, b"expectDoc") else {
        return Err("missing"); // :1932
    };
    if params.get(ed) != Some(&b'{') {
        return Err("missing");
    }
    let object = &params[ed..];
    let mut idbuf = [0u8; 32];
    if !json_string(object, b"id", &mut idbuf) {
        return Err("missing_id"); // :1937
    }
    let Some(epoch_in) = json_int(object, b"epoch") else {
        return Err("missing_epoch"); // :1938
    };
    // :1939-1940: every digit of the id, wherever it sits in the string ("d1" -> 1).
    let mut want: u32 = 0;
    for &byte in cstr(&idbuf) {
        if byte.is_ascii_digit() {
            want = want
                .wrapping_mul(10)
                .wrapping_add(u32::from(byte - b'0'));
        }
    }
    if want != doc_seq {
        return Err("doc_id"); // :1941
    }
    if i64::from(epoch) != epoch_in {
        return Err("epoch"); // :1942
    }
    Ok(())
}

/// `sb_add_doc_tail` (`mcp_bridge.c:1878-1889`): splice `,"doc":{...}` in before the result
/// object's closing `}`.
///
/// The C rewinds its builder's last byte (`out->len--`) and appends the tail. A builder whose last
/// byte is not `}` (or that holds nothing) is left untouched and the C's line is logged, so a
/// shape surprise can never turn into a wrong tail.
pub(crate) fn doc_tail_into(out: &mut Sb, doc: Option<Doc>) {
    let Some(doc) = doc else {
        return;
    };
    if out.bytes().last() != Some(&b'}') {
        log::line("doc tail: the result is not a complete object; no doc state added"); // :1884
        return;
    }
    out.truncate(out.bytes().len() - 1);
    out.add(b",\"doc\":");
    sb_add_doc_object(out, doc);
    out.add(b"}");
}

// ---------------------------------------------------------------------------------------------
// `entity.create` (`mcp_bridge.c:2063-2170`)
// ---------------------------------------------------------------------------------------------

/// The validated inputs of an `entity.create` (`CreateTarget`, `mcp_bridge.c:2063`): the class
/// name, the origin/angles vectors and the optional scales. Resolving the class in Hammer's node
/// factory is the one game read in the C's `entity_create_target`; it stays in
/// `entity_create_with`, so this part is a plain function over the params.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CreateTarget {
    /// `cls` (64 bytes).
    pub cls: Vec<u8>,
    /// `origin` (zeros when absent).
    pub origin: [f32; 3],
    /// `angles` (zeros when absent).
    pub angles: [f32; 3],
    /// `scales` (`1,1,1` when absent).
    pub scales: [f32; 3],
    /// `has_scales`: the caller sent three scales, so the create writes them.
    pub has_scales: bool,
}

/// `entity_create_target`'s parameter part (`mcp_bridge.c:2069-2111`): class, origin, angles,
/// scales, in the C's check order. The class registry lookup follows in the op.
pub(crate) fn create_target_parse(params: Option<&str>) -> Result<CreateTarget, OpErr> {
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "class"));
    };
    let bytes = params.as_bytes();
    let mut cls = [0u8; 64];
    if !json_string_top(bytes, b"class", &mut cls) || cls[0] == 0 {
        return Err(OpErr::new(-32602, "InvalidParams", "class")); // :2069
    }
    let origin = json_float3_top(bytes, b"origin");
    let angles = json_float3_top(bytes, b"angles");
    let scales = json_float3_top(bytes, b"scales");
    if json_find_top(bytes, b"origin").is_some() && origin.is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "origin: [x,y,z] required")); // :2080
    }
    if json_find_top(bytes, b"angles").is_some() && angles.is_none() {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "angles: [pitch,yaw,roll] required",
        )); // :2084
    }
    if json_find_top(bytes, b"scales").is_some() && scales.is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "scales: [x,y,z] required")); // :2090
    }
    let has_scales = scales.is_some();
    let target = CreateTarget {
        cls: cstr(&cls).to_vec(),
        origin: origin.unwrap_or([0.0; 3]),
        angles: angles.unwrap_or([0.0; 3]),
        scales: scales.unwrap_or([1.0; 3]),
        has_scales,
    };
    if has_scales && !xform_scales_ok(&target.scales) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "scales: each finite and in (0, 1000]",
        )); // :2093
    }
    Ok(target)
}

/// `entity_create_apply`'s result object (`mcp_bridge.c:2113-2157`), byte for byte. `handle` is
/// the token [`handle_new`] answered, `node_id` the created node's own id when `node_id_of`
/// succeeded, and `keyvalues` the applied-keyvalues result when the caller sent `keyValues` and
/// the node has a properties element.
///
/// Returns the auxiliary-buffer overflow (`apply_keyvalues`'s `out->ovf = 1`, `:2418`).
pub(crate) fn create_reply_into(
    out: &mut Sb,
    handle: &[u8],
    node_id: Option<i32>,
    target: &CreateTarget,
    keyvalues: Option<&KvApply>,
) -> bool {
    out.add(b"{\"handle\":");
    out.addjson(handle);
    if let Some(id) = node_id {
        out.add(b",\"nodeID\":");
        out.addi(i64::from(id));
    }
    out.add(b",\"class\":");
    out.addjson(&target.cls);
    out.add(b",\"origin\":");
    addf3(out, target.origin);
    out.add(b",\"scales\":");
    addf3(out, target.scales);
    out.add(b",\"undoLabel\":\"MCP: create entity\"");
    let mut aux = false;
    if let Some(keyvalues) = keyvalues {
        out.add(b",\"keyValues\":");
        aux = kv_apply_into(out, keyvalues, false);
    }
    out.add(b"}");
    aux
}

// ---------------------------------------------------------------------------------------------
// `apply_keyvalues` (`mcp_bridge.c:2313-2424`)
// ---------------------------------------------------------------------------------------------

/// `kv_previous_json`'s answer for one attribute (`mcp_bridge.c:2132-2147`).
#[allow(dead_code)] // constructed by the Windows reader and the host tests' fake
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PrevValue {
    /// `null`: no attribute, an unreadable one, or a value the four setters do not report.
    Null,
    /// `null` **and** the name goes into `previousTruncated` (`probe_str` answered -1).
    Cut,
    /// A String value (raw bytes; [`Sb::addjson`] does the C's escaping).
    Str(Vec<u8>),
    /// `S2_AT_INT`.
    Int(i32),
    /// `S2_AT_FLOAT`.
    Float(f32),
    /// `S2_AT_BOOL`.
    Bool(bool),
}

/// One refused key's `warnings` entry (`mcp_bridge.c:2305-2424`). The text is built in
/// [`kv_warning_text`] at the C's own buffer size (192 bytes, or 384 for the case-variant entry).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum KvWarning {
    /// `name: value longer than KV_VALUE_MAX bytes` (`:2340-2344`, `over == 1`).
    TooLong,
    /// `name: attribute not readable` (`:2346-2350`, a dangling attribute pointer).
    AttrUnreadable,
    /// `name: number out of range for this attribute` (`:2351-2364`, `over == 2` on a typed
    /// attribute).
    NumberOutOfRange,
    /// `name: refused, the entity spells this key "<variant>" (...)` (`kv_warn_variant`,
    /// `:2018-2028`).
    Variant(Vec<u8>),
    /// `name: number out of range for an int attribute` (`:2400-2405`).
    IntOutOfRange,
    /// `name: attribute type <n> not writable` (`:2430-2435`).
    NotWritable(i32),
}

/// One key of `apply_keyvalues`'s `applied` object: the name and whether a setter ran, plus the
/// one warning the key may have produced (`applied:false` in that case).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KvEntry {
    /// The key's name, as `kv_next` decoded it.
    pub name: Vec<u8>,
    /// `applied[name]`.
    pub applied: bool,
    /// The `warnings` entry, in key order.
    pub warning: Option<KvWarning>,
}

/// `apply_keyvalues`'s whole result (`mcp_bridge.c:2416-2424`), before it is rendered.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct KvApply {
    /// One entry per key, in the caller's order.
    pub entries: Vec<KvEntry>,
    /// The `previous` pass when the caller asked for it (`entity.setKeyValues`).
    pub previous: Option<Vec<(Vec<u8>, PrevValue)>>,
    /// The C's `kv: parser stalled` early return (`:2221`, `:2242`): nothing is rendered and the
    /// op answers its overflow error.
    pub stalled: bool,
}

/// The attribute facts `apply_keyvalues`' write pass reads once per key (`mcp_bridge.c:2330-2340`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AttrInfo {
    /// `sym.m_pString != 0`: the data model interned the name, so a missing key is creatable.
    pub symbol: bool,
    /// The `CDmAttribute *` (`0` when `FindAttribute` answered null).
    pub attr: usize,
    /// `attr != 0`.
    pub present: bool,
    /// `mem_ok(attr) && mem_ok(attr + sizeof - 1)`.
    pub readable: bool,
    /// `typeFlags & S2_ATTR_TYPE_MASK`, `-1` when the attribute is missing or unreadable.
    pub kind: i32,
}

/// `kv_warning_text`: one warning's text, built at the C's buffer size. The case-variant entry is
/// the one built in 384 bytes (`kv_warn_variant`, `:2021`); every other entry uses 192 bytes
/// (`:2341`, `:2347`, `:2352`, `:2410`), so an over-long name is cut there and never overflows
/// the `warnings` array itself.
fn kv_warning_text(name: &[u8], warning: &KvWarning) -> Vec<u8> {
    let cap = match warning {
        KvWarning::Variant(_) => 384,
        _ => 192,
    };
    let mut buf = vec![0u8; cap];
    let mut w = Sb::new(&mut buf);
    w.add(name);
    match warning {
        KvWarning::TooLong => {
            w.add(b": value longer than ");
            w.addi(i64::from(crate::entity_json::KV_VALUE_MAX as i32));
            w.add(b" bytes");
        }
        KvWarning::AttrUnreadable => w.add(b": attribute not readable"),
        KvWarning::NumberOutOfRange => w.add(b": number out of range for this attribute"),
        KvWarning::Variant(variant) => {
            w.add(b": refused, the entity spells this key \"");
            w.add(variant);
            w.add(b"\" (SetKeyValue matches case-insensitively, `previous` is exact)");
        }
        KvWarning::IntOutOfRange => w.add(b": number out of range for an int attribute"),
        KvWarning::NotWritable(kind) => {
            w.add(b": attribute type ");
            w.addi(i64::from(*kind));
            w.add(b" not writable");
        }
    }
    cstr(&buf).to_vec()
}

/// `apply_keyvalues`'s result object (`mcp_bridge.c:2416-2424`), byte for byte.
///
/// The four scratch builders are the C's statics - `abuf[8192]`, `wbuf[4096]`, `pbuf[65536]`,
/// `tbuf[2048]` - so an over-long `applied` / `warnings` / `previous` list overflows at exactly
/// the same point and the op answers its `-32603` overflow message instead of emitting cut JSON.
/// Returns that overflow.
pub(crate) fn kv_apply_into(out: &mut Sb, apply: &KvApply, with_previous: bool) -> bool {
    if apply.stalled {
        // Nothing is rendered: the C sets `out->ovf = 1` and returns before adding anything
        // (`:2221`, `:2242`). The stall itself was already logged at its site, by the pass that
        // hit it (`apply_keyvalues_with`) - the C logs there and only there.
        return true;
    }
    let mut abuf = vec![0u8; 8192];
    let mut wbuf = vec![0u8; 4096];
    let mut pbuf = vec![0u8; 65536];
    let mut tbuf = vec![0u8; 2048];
    let mut ap = Sb::new(&mut abuf);
    let mut wn = Sb::new(&mut wbuf);
    let mut pv = Sb::new(&mut pbuf);
    let mut tn = Sb::new(&mut tbuf);

    for entry in &apply.entries {
        if !ap.bytes().is_empty() {
            ap.add(b",");
        }
        ap.addjson(&entry.name);
        ap.add(b":");
        ap.add(if entry.applied { b"true" } else { b"false" });
    }
    for entry in &apply.entries {
        let Some(warning) = entry.warning.as_ref() else {
            continue;
        };
        let msg = kv_warning_text(&entry.name, warning);
        // The C drops every later warning once the array overflowed (`:2427`).
        if !wn.ovf() {
            if !wn.bytes().is_empty() {
                wn.add(b",");
            }
            wn.addjson(&msg);
        }
    }
    if with_previous {
        let mut first = true;
        for (name, value) in apply.previous.as_deref().unwrap_or(&[]) {
            if !first {
                pv.add(b",");
            }
            first = false;
            pv.addjson(name);
            pv.add(b":");
            match value {
                PrevValue::Null => pv.add(b"null"),
                PrevValue::Cut => {
                    pv.add(b"null");
                    if !tn.bytes().is_empty() {
                        tn.add(b",");
                    }
                    tn.addjson(name);
                }
                PrevValue::Str(text) => pv.addjson(text),
                PrevValue::Int(value) => pv.addi(i64::from(*value)),
                PrevValue::Float(value) => pv.addf(*value),
                PrevValue::Bool(value) => pv.add(if *value { b"true" } else { b"false" }),
            }
        }
    }

    let aux = ap.ovf() || wn.ovf() || pv.ovf() || tn.ovf(); // :2417
    out.add(b"{\"applied\":{");
    out.add(ap.as_cstr());
    out.add(b"},\"warnings\":[");
    out.add(wn.as_cstr());
    out.add(b"]");
    if with_previous {
        out.add(b",\"previous\":{");
        out.add(pv.as_cstr());
        out.add(b"},\"previousTruncated\":[");
        out.add(tn.as_cstr());
        out.add(b"],\"valueUndo\":\"recorded\"");
    }
    out.add(b"}");
    aux
}

/// The text a String-attribute write stores (`mcp_bridge.c:2385-2387`, `:2439-2441`): a string
/// value as itself, a bool as `"1"`/`"0"`, a number as its literal token.
fn kv_write_text(kind: i32, sval: &[u8], nval: &[u8], ival: i64) -> Vec<u8> {
    if kind == crate::entity_json::KV_STRING {
        cstr(sval).to_vec()
    } else if kind == crate::entity_json::KV_BOOL {
        if ival != 0 {
            b"1".to_vec()
        } else {
            b"0".to_vec()
        }
    } else {
        cstr(nval).to_vec()
    }
}

/// `apply_keyvalues` (`mcp_bridge.c:2313-2424`): parse the `{name:value,...}` object at `vp` in
/// `params`, decide every key and run the setters, in the C's order.
///
/// The decisions (`applied`, `warnings`, `previous`) are plain values, so
/// [`kv_apply_into`] renders them byte for byte in the host tests; every game read or write is one
/// [`Write`] call. The parameter order and the C line of each step are in the body.
pub(crate) fn apply_keyvalues_with(
    w: &dyn Write,
    node: Node,
    element: usize,
    params: &[u8],
    vp: usize,
    with_previous: bool,
) -> KvApply {
    let mut apply = KvApply::default();
    let mut name = [0u8; KV_NAME_CAP];
    let mut sval = vec![0u8; crate::entity_json::KV_VALUE_MAX + 1];
    let mut nval = [0u8; KV_NAME_CAP];

    if with_previous {
        // :2318-2333: a pass of its own, BEFORE anything is written.
        let mut previous = Vec::new();
        let mut p = vp + 1;
        loop {
            let before = p;
            let Some(value) = crate::entity_json::kv_next(
                params, p, &mut name, &mut sval, &mut nval,
            ) else {
                break;
            };
            p = value.next;
            if p == before {
                log::line("kv: parser stalled (previous pass)"); // :2221
                apply.stalled = true;
                return apply;
            }
            let key = cstr(&name).to_vec();
            previous.push((key.clone(), w.attr_previous(element, &key)));
        }
        apply.previous = Some(previous);
    }

    let mut p = vp + 1;
    loop {
        let before = p;
        let Some(value) =
            crate::entity_json::kv_next(params, p, &mut name, &mut sval, &mut nval)
        else {
            break;
        };
        p = value.next;
        if p == before {
            log::line("kv: parser stalled"); // :2242
            apply.stalled = true;
            return apply;
        }
        let key = cstr(&name).to_vec();
        let info = w.attr_info(element, &key); // :2365-2368
        let mut entry = KvEntry {
            name: key.clone(),
            applied: false,
            warning: None,
        };
        if value.over == 1 {
            // :2338-2344: never write a cut value.
            entry.warning = Some(KvWarning::TooLong);
        } else if info.present && !info.readable {
            // :2346-2350: a dangling attribute pointer.
            entry.warning = Some(KvWarning::AttrUnreadable);
        } else if value.over == 2
            && matches!(
                info.kind as u32,
                AT_AT_INT | AT_AT_BOOL | AT_AT_FLOAT
            )
        {
            // :2351-2364: the token is outside what this attribute's setter can hold.
            entry.warning = Some(KvWarning::NumberOutOfRange);
        } else if info.present {
            match info.kind as u32 {
                AT_AT_STRING => {
                    // :2380-2394: Hammer's own SetKeyValue, gated on the case-variant rule.
                    match w.case_variant(element, &key) {
                        Some(variant) => entry.warning = Some(KvWarning::Variant(variant)),
                        None => {
                            let text = kv_write_text(value.kind, &sval, &nval, value.ival);
                            w.set_key_value(node, &key, &text);
                            entry.applied = true;
                        }
                    }
                }
                AT_AT_INT | AT_AT_BOOL => {
                    // :2395-2410
                    if value.kind == crate::entity_json::KV_INT
                        || value.kind == crate::entity_json::KV_BOOL
                    {
                        if value.ival > 2147483647 || value.ival < -2147483648 {
                            entry.warning = Some(KvWarning::IntOutOfRange);
                        } else {
                            w.set_value_int(info.attr, value.ival as i32);
                            entry.applied = true;
                        }
                    } else {
                        log::line("kv: type mismatch (attr vs json)");
                    }
                }
                AT_AT_FLOAT => {
                    // :2411-2418
                    if value.kind == crate::entity_json::KV_FLOAT {
                        w.set_value_float(info.attr, value.fval as f32);
                        entry.applied = true;
                    } else {
                        log::line("kv: type mismatch (attr vs json)");
                    }
                }
                other => {
                    // :2419-2426: an attribute type the four DME setters cannot write.
                    entry.warning = Some(KvWarning::NotWritable(other as i32));
                }
            }
        } else if info.symbol {
            // :2427-2446: a key the entity does not have - SetKeyValue creates it as a String.
            match w.case_variant(element, &key) {
                Some(variant) => entry.warning = Some(KvWarning::Variant(variant)),
                None => {
                    let text = kv_write_text(value.kind, &sval, &nval, value.ival);
                    w.set_key_value(node, &key, &text);
                    entry.applied = true;
                }
            }
        }
        apply.entries.push(entry);
    }
    apply
}

// ---------------------------------------------------------------------------------------------
// `entity.addConnections` / `entity.removeConnections` (`mcp_bridge.c:3714-3997`)
// ---------------------------------------------------------------------------------------------

/// One wire entry's outcome in `entity.addConnections` (`mcp_bridge.c:3751-3795`): the call's
/// answer plus the live vector's read-back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConnAdd {
    /// The C's `ok`: the call reported `index << 16`, the count grew by exactly one, and the
    /// entry at that index reads back.
    pub ok: bool,
    /// `landed` in the C's sense: the call reported a valid index (`None` when it reported none).
    pub reported: Option<i32>,
    /// The live entry at `reported`, when `ok`.
    pub conn: Option<Conn>,
}

/// An `entity.addConnections` reply (`mcp_bridge.c:3772-3810`), byte for byte. Returns the
/// `ab` scratch-builder overflow (the C's `if (ab.ovf) out->ovf = 1`, `:3797`).
pub(crate) fn add_connections_into(out: &mut Sb, id: i64, results: &[ConnAdd]) -> bool {
    // Worst case per entry: four 255-byte names, each byte a 2-byte JSON escape, plus the index
    // and the numbers (~2.2 KB).
    let mut abuf = vec![0u8; crate::entity_json::CONN_MAX * 2304];
    let mut ab = Sb::new(&mut abuf);
    for (index, result) in results.iter().enumerate() {
        if index != 0 {
            ab.add(b","); // :3780
        }
        match (result.ok, result.reported, result.conn.as_ref()) {
            (true, Some(landed), Some(conn)) => {
                ab.add(b"{\"index\":");
                ab.addi(i64::from(landed));
                ab.add(b",");
                conn_into(&mut ab, conn);
                ab.add(b"}");
            }
            _ => {
                ab.add(b"{\"readback\":false");
                if let Some(reported) = result.reported {
                    ab.add(b",\"index\":");
                    ab.addi(i64::from(reported));
                }
                ab.add(b"}");
            }
        }
    }
    let aux = ab.ovf();
    let confirmed = results.iter().filter(|result| result.ok).count();
    out.add(b"{\"nodeID\":");
    out.addi(id);
    out.add(b",\"added\":[");
    out.add(ab.as_cstr());
    out.add(b"]");
    out.add(b",\"count\":");
    out.addi(confirmed as i64);
    out.add(b",\"landed\":[");
    for (index, result) in results.iter().enumerate() {
        if index != 0 {
            out.add(b",");
        }
        out.add(b"{\"index\":");
        match result.reported {
            Some(reported) => out.addi(i64::from(reported)),
            None => out.add(b"null"),
        }
        out.add(b",\"landed\":");
        out.add(if result.ok { b"true" } else { b"false" });
        out.add(b"}");
    }
    out.add(b"],\"undoLabel\":\"MCP: add connections\"}");
    aux
}

/// An `entity.removeConnections` reply (`mcp_bridge.c:3963-3981`), byte for byte.
/// `removed` is in the C's walk order (descending indices); the reply lists them ascending, and
/// `count` was read from the vector before the group opened.
pub(crate) fn remove_connections_into(out: &mut Sb, id: i64, removed: &[i32], count: i64) {
    out.add(b"{\"nodeID\":");
    out.addi(id);
    out.add(b",\"removed\":[");
    for (index, value) in removed.iter().rev().enumerate() {
        if index != 0 {
            out.add(b",");
        }
        out.addi(i64::from(*value));
    }
    out.add(b"],\"count\":");
    out.addi(removed.len() as i64);
    out.add(b",\"remaining\":");
    out.addi(count - removed.len() as i64);
    out.add(b",\"undoLabel\":\"MCP: remove connections\"}");
}

/// The `match` filter `entity.removeConnections` resolves against the live vector
/// (`mcp_bridge.c:3944-3955`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConnFilter {
    /// `has_mo` + the value.
    pub output: Option<Vec<u8>>,
    /// `has_mt` + the value.
    pub target: Option<Vec<u8>>,
    /// `has_mi` + the value.
    pub input: Option<Vec<u8>>,
}

/// The three strings of one live connection that copied whole (`conn_copy_str`,
/// `mcp_bridge.c:3606-3611`): `None` is the C's `0` answer, and a connection whose value did not
/// copy whole never matches a filter.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FilterFields {
    /// `output`.
    pub output: Option<Vec<u8>>,
    /// `target`.
    pub target: Option<Vec<u8>>,
    /// `input`.
    pub input: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------------------------
// `node.delete` / `node.setTransform` (`mcp_bridge.c:5475-5611`)
// ---------------------------------------------------------------------------------------------

/// `jws` (`mcp_bridge.c:3162`): skip the whitespace the C's JSON readers allow.
pub(crate) fn jws(s: &[u8], mut p: usize) -> usize {
    while matches!(byte_at(s, p), b' ' | b'\n' | b'\t' | b'\r') {
        p += 1;
    }
    p
}

/// One byte of a JSON buffer, `0` past its end (the C's NUL).
pub(crate) fn byte_at(s: &[u8], p: usize) -> u8 {
    s.get(p).copied().unwrap_or(0)
}

/// `jnum` (`mcp_bridge.c:3136-3158`): a number with optional fraction/exponent, whitespace
/// skipped first. Answers the position after the token and the value. The exponent is bounded to
/// 300 (`ex > 300` fails) and a bare `e` is malformed, exactly as the C reads it.
pub(crate) fn jnum(s: &[u8], start: usize) -> Option<(usize, f64)> {
    let mut p = jws(s, start);
    let mut negative = false;
    if byte_at(s, p) == b'-' {
        negative = true;
        p += 1;
    }
    if !byte_at(s, p).is_ascii_digit() {
        return None;
    }
    let mut value = 0.0f64;
    while byte_at(s, p).is_ascii_digit() {
        value = value * 10.0 + f64::from(byte_at(s, p) - b'0');
        p += 1;
    }
    if byte_at(s, p) == b'.' {
        p += 1;
        let mut fraction = 0.1f64;
        while byte_at(s, p).is_ascii_digit() {
            value += f64::from(byte_at(s, p) - b'0') * fraction;
            fraction *= 0.1;
            p += 1;
        }
    }
    if matches!(byte_at(s, p), b'e' | b'E') {
        p += 1;
        let mut exponent_negative = false;
        let mut exponent: i32 = 0;
        if byte_at(s, p) == b'+' {
            p += 1;
        } else if byte_at(s, p) == b'-' {
            exponent_negative = true;
            p += 1;
        }
        if !byte_at(s, p).is_ascii_digit() {
            return None;
        }
        while byte_at(s, p).is_ascii_digit() {
            exponent = exponent * 10 + i32::from(byte_at(s, p) - b'0');
            p += 1;
            if exponent > 300 {
                return None;
            }
        }
        while exponent > 0 {
            value = if exponent_negative {
                value / 10.0
            } else {
                value * 10.0
            };
            exponent -= 1;
        }
    }
    Some((p, if negative { -value } else { value }))
}

/// `op_node_delete`'s result object (`mcp_bridge.c:5511-5514`), byte for byte.
pub(crate) fn node_delete_into(out: &mut Sb, ids: &[i32]) {
    out.add(b"{\"deleted\":[");
    for (index, id) in ids.iter().enumerate() {
        if index != 0 {
            out.add(b",");
        }
        out.addi(i64::from(*id));
    }
    out.add(b"],\"undoLabel\":\"MCP: delete nodes\"}");
}

/// `op_node_set_transform`'s result object (`mcp_bridge.c:5601-5607`), byte for byte: the values
/// are read back from the node, so an unreadable field answers `null`.
pub(crate) fn set_transform_into(
    out: &mut Sb,
    id: i64,
    class: &str,
    origin: Option<[f32; 3]>,
    angles: Option<[f32; 3]>,
    scales: Option<[f32; 3]>,
) {
    out.add(b"{\"nodeID\":");
    out.addi(id);
    out.add(b",\"class\":\"");
    out.add(class.as_bytes());
    out.add(b"\"");
    out.add(b",\"origin\":");
    match origin {
        Some(value) => addf3(out, value),
        None => out.add(b"null"),
    }
    out.add(b",\"angles\":");
    match angles {
        Some(value) => addf3(out, value),
        None => out.add(b"null"),
    }
    out.add(b",\"scales\":");
    match scales {
        Some(value) => addf3(out, value),
        None => out.add(b"null"),
    }
    out.add(b",\"undoLabel\":\"MCP: set transform\"}");
}

// ---------------------------------------------------------------------------------------------
// `undo.undo` / `undo.redo` (`mcp_bridge.c:5747-5812`)
// ---------------------------------------------------------------------------------------------

/// An `undo.undo` / `undo.redo` reply, byte for byte. `popped` is `None` for the empty-stack
/// result (`undone:false,name:null`), else the popped group's label, whether the stack shrank and
/// the depth after the call. `doc_object` is the `{"id":...}` text `sb_add_doc_object` builds.
pub(crate) fn undo_reply(
    id: &str,
    redo: bool,
    popped: Option<(&[u8], bool, u32)>,
    doc_object: &str,
) -> String {
    let mut buf = vec![0u8; control::MAX_LINE as usize];
    let mut b = Sb::new(&mut buf);
    b.add(b"{\"jsonrpc\":\"2.0\",\"id\":");
    b.addjson(id.as_bytes());
    b.add(b",\"bridge\":{\"protocol\":1,\"build\":\"");
    b.add(crate::BRIDGE_BUILD.as_bytes());
    b.add(b"\"},\"result\":{");
    match popped {
        None => {
            // :5768-5769: an empty stack is a RESULT, never an error.
            b.add(if redo {
                b"\"redone\":false,\"name\":null"
            } else {
                b"\"undone\":false,\"name\":null"
            });
        }
        Some((name, undone, depth)) => {
            b.add(if redo {
                b"\"redone\":"
            } else {
                b"\"undone\":"
            });
            b.add(if undone { b"true" } else { b"false" });
            b.add(b",\"name\":");
            b.addjson(name);
            if redo {
                b.add(b",\"redoDepth\":");
            } else {
                b.add(b",\"undoDepth\":");
            }
            b.addnum(u64::from(depth));
        }
    }
    b.add(b",\"doc\":");
    b.add(doc_object.as_bytes());
    b.add(b"}}\n");
    if b.ovf() {
        log::line("exec: response buffer overflow"); // the C's exec_request tail
    }
    String::from_utf8_lossy(b.as_cstr()).into_owned()
}

// ---------------------------------------------------------------------------------------------
// The write primitives and the op bodies (`mcp_bridge.c:1834-1849`, `:1938-2468`, `:3714-3997`,
// `:5475-5611`)
// ---------------------------------------------------------------------------------------------

/// The mutating half of the live Hammer state: one method per C primitive the write ops reach.
///
/// The host tests implement it with a fake that records every call (the arguments are asserted
/// against the C's call sites), while [`win::LiveHammer`] binds each to a generated vtable slot or
/// a pinned `HFN_*` entry. `Chain` supplies the document identity and `Nodes` the tree reads.
pub(crate) trait Write: Chain + Nodes {
    /// `g_hfn.FindTypeByName(registry, cls) != 0` (`mcp_bridge.c:2101`).
    fn class_exists(&self, cls: &[u8]) -> bool;
    /// `CreateNodeByName` + the `SetScales` vtable write the C does right after it
    /// (`mcp_bridge.c:2101-2110`). `target` is `&mut` because the C's prototype takes the arrays as
    /// `float *` (`s2sdk.h:704`) and prints them AFTER the call (`:2122-2129`), so a
    /// write-back by Hammer lands in the very storage the reply is built from.
    fn create_node(&self, doc: Doc, target: &mut CreateTarget) -> Option<Node>;
    /// `g_hfn.GetEntityProperties(node)` with the C's non-null guard (`:2372-2375`, `:2208`).
    fn properties(&self, node: Node) -> Option<usize>;
    /// `undo_open` (`mcp_bridge.c:1834-1839`): a named group on the active history, returned as
    /// the history token.
    ///
    /// `label` is `&'static CStr` because every C caller passes a string literal
    /// (`"MCP: create mesh"`, and `batch_undo_label`'s two literals): the pointer is static
    /// storage Hammer may keep, so building a temporary here would leave it dangling.
    fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize;
    /// `RecordNodeTree(hist, CMapNode_Undoable(node), 1)`.
    fn record_node_tree(&self, hist: usize, node: Node);
    /// `DocSetModified(doc, doc)` (`:1310`, `:2449`).
    fn doc_set_modified(&self, doc: Doc);
    /// `g_hfn.GetActiveHistory()`.
    fn get_active_history(&self) -> usize;
    /// `g_hfn.GetRedoHistory(doc)`.
    fn get_redo_history(&self, doc: Doc) -> usize;
    /// `obj_is(hist, PIN_HAMMER, HV_HISTORY_VT)` (`:5762`).
    fn history_ok(&self, hist: usize) -> bool;
    /// `history_top_info(hist)` (`:1810-1824`).
    fn history_info(&self, hist: usize) -> HistoryInfo;
    /// `hist->vt->Undo(hist)` (`:5773`).
    fn undo_pop(&self, hist: usize);
    /// `g_hfn.NodeIsLocked(node, 0)` (`:2211`, `:5497`).
    fn node_locked(&self, node: Node) -> bool;
    /// `g_hfn.DeleteNode(doc, node)`.
    fn delete_node(&self, doc: Doc, node: Node);
    /// `node->vt->SetOrigin(node, origin)` (slot 17).
    fn set_origin(&self, node: Node, value: [f32; 3]);
    /// `node->vt->SetAngles(node, angles)` (slot 18).
    fn set_angles(&self, node: Node, value: [f32; 3]);
    /// `node->vt->SetScales(node, scales)` (slot 19).
    fn set_scales(&self, node: Node, value: [f32; 3]);
    /// `node_angles` (`:3296-3301`).
    fn angles(&self, node: Node) -> Option<[f32; 3]>;
    /// `node_scales` (`:3303-3308`).
    fn scales(&self, node: Node) -> Option<[f32; 3]>;
    /// `kv_previous_json` over one key (`:2132-2147`): intern, `FindAttribute`, read the value.
    fn attr_previous(&self, element: usize, name: &[u8]) -> PrevValue;
    /// The write pass's per-key reads (`:2365-2372`): intern, `FindAttribute`, `typeFlags`.
    fn attr_info(&self, element: usize, name: &[u8]) -> AttrInfo;
    /// `kv_case_variant` (`:2029-2037`): the attribute spelling that matches only
    /// case-insensitively, when there is one.
    fn case_variant(&self, element: usize, name: &[u8]) -> Option<Vec<u8>>;
    /// `g_hfn.SetKeyValue(node, name, value)` (`:2388`, `:2442`).
    fn set_key_value(&self, node: Node, name: &[u8], value: &[u8]);
    /// `g_hfn.SetValueInt(attr, &value)`.
    fn set_value_int(&self, attr: usize, value: i32);
    /// `g_hfn.SetValueFloat(attr, &value)`.
    fn set_value_float(&self, attr: usize, value: f32);
    /// `conn_vec`'s count (`:3555-3569`): `None` is the C's refusal.
    fn conn_count(&self, node: Node) -> Option<usize>;
    /// `conn_at` + `conn_entry_ok` + the `sb_add_conn_fields` reads (`:3571-3581`).
    fn conn_entry(&self, node: Node, index: usize) -> Option<Conn>;
    /// `conn_copy_str` over the three filterable strings (`:3606-3611`).
    fn conn_filter_fields(&self, node: Node, index: usize) -> Option<FilterFields>;
    /// The `AddConnection` + `Kv3Init`/`Kv3Free` recipe (`:3768-3776`): `(landed, index)`.
    fn add_connection(&self, node: Node, spec: &ConnSpec) -> (bool, Option<i32>);
    /// `g_hfn.RemoveConnection(node, index << 16) != 0` (`:3960`).
    fn remove_connection(&self, node: Node, index: i32) -> bool;
}

/// The C's `geo_dispatch` head for a mutating op (`mcp_bridge.c:5615-5645`, and the duplicated
/// heads of `entity.create`/`entity.setKeyValues`/`undo.*` at `:5705-5718`, `:5813-5822`):
/// the active document, the full `doc_editable` set, then the caller's `expectDoc`.
///
/// The order is the C's: a request to a document that cannot be edited answers
/// `-32018 DocumentNotEditable` before its params are looked at, and `-32003 NoActiveDocument`
/// before that.
pub(crate) fn write_gate(chain: &dyn Chain, params: Option<&[u8]>) -> Result<Doc, OpErr> {
    let doc = active_doc_with(chain).map_err(|reason| {
        OpErr::new(-32003, "NoActiveDocument", reason)
    })?;
    let (editable, why) = doc_editable_with(chain, doc);
    if !editable {
        return Err(OpErr::new(-32018, "DocumentNotEditable", why));
    }
    let (doc_seq, epoch) = fingerprint_ids();
    expect_doc_ok(params, doc_seq, epoch)
        .map_err(|why| OpErr::new(-32004, "DocumentChanged", why))?;
    Ok(doc)
}

/// `entity.create`'s arm half (`mcp_bridge.c:5720-5751`): the arm opens the undo group.
///
/// The C's `op_entity_create` (`:2167-2171`) opens NO group - the standalone arm (`:5726`) does,
/// and `edit.batch` (`:5901`) calls the same body while its own group (`:5878`) is already open.
/// Hence the two wrappers over [`entity_create_with`]: this one and
/// [`op_entity_create_no_group`].
pub(crate) fn op_entity_create(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    entity_create_with(w, doc, params, out, Some(c"MCP: create entity"))
}

/// `edit.batch`'s per-op `entity.create` (`mcp_bridge.c:2167-2171`, called at `:5901`): the same
/// body WITHOUT an undo group — the batch opened ONE for the whole run (`:5878`).
pub(crate) fn op_entity_create_no_group(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    entity_create_with(w, doc, params, out, None)
}

/// `entity_create_target` + `entity_create_apply` (`mcp_bridge.c:2049-2146`): the class lookup,
/// the optional undo group, the node, the reply. `group` is the C's `undo_open` label: `Some` for
/// the standalone arm (`:5726`), `None` for `edit.batch` (`:2167-2171` opens nothing).
fn entity_create_with(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
    group: Option<&'static CStr>,
) -> Result<bool, OpErr> {
    let mut target = create_target_parse(params)?;
    if !w.class_exists(&target.cls) {
        // :2101-2104
        return Err(OpErr::new(
            -32006,
            "UnknownClass",
            "not in the CMapNodeFactory registry",
        ));
    }
    // :5730: the group opens only after the whole target validated; `edit.batch` (`:2167-2171`)
    // passes no label, its own group is already open (`:5878`).
    if let Some(label) = group {
        w.undo_open(doc, label);
    }
    let Some(node) = w.create_node(doc, &mut target) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "CreateNodeByName failed (see bridge log)",
        )); // :2102-2104
    };
    let handle = handle_new(node.0); // :2111
    log::line("entity: created "); // :2112-2113
    log::line(&String::from_utf8_lossy(&target.cls));
    // :2120: the created node's nodeID is read BEFORE the keyValues block below, as the C does
    // (`node_id_of` sits above its `GetEntityProperties` at :2133). The reply's `origin`/`angles`
    // come from the same `target` storage the create call was handed as `float *`
    // (`:2122-2129`), so a write-back by Hammer shows in them.
    let node_id = w.id(node);
    let mut keyvalues = None;
    if let Some(params) = params {
        let bytes = params.as_bytes();
        if let Some(at) = json_find_top(bytes, b"keyValues") {
            if byte_at(bytes, at) == b'{' {
                // :2131-2136: only a node with a properties element carries the echo.
                if let Some(element) = w.properties(node) {
                    keyvalues = Some(apply_keyvalues_with(w, node, element, bytes, at, false));
                }
            }
        }
    }
    Ok(create_reply_into(
        out,
        handle.as_bytes(),
        node_id,
        &target,
        keyvalues.as_ref(),
    ))
}

/// `set_keyvalues_target` (`mcp_bridge.c:2470-2524`): the resolved node, its properties element
/// and the `values` object's position.
pub(crate) struct SetKvTarget {
    /// The node `SetKeyValue`/`RecordNodeTree` receive.
    pub node: Node,
    /// `GetEntityProperties(node)`.
    pub element: usize,
    /// The offset of `values`' `{` in the params.
    pub vp: usize,
}

/// `set_keyvalues_target` (`mcp_bridge.c:2470-2524`): validate the handle/nodeID form, the node,
/// the lock and the `values` member, before the undo group exists.
pub(crate) fn set_keyvalues_target(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
) -> Result<SetKvTarget, OpErr> {
    let mut htok = [0u8; 32];
    let mut nid = 0i64;
    let bytes = params.map(str::as_bytes);
    let has_handle = bytes.is_some_and(|b| json_string_top(b, b"handle", &mut htok));
    let has_node = bytes.is_some_and(|b| json_find_top(b, b"nodeID").is_some());
    if has_node {
        match json_int_top(bytes.unwrap(), b"nodeID") {
            Some(value) => nid = value,
            None => {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "nodeID: a non-negative int up to 2147483647 is required",
                ))
            }
        }
    }
    if has_handle == has_node {
        // :2496-2500: exactly one of handle/nodeID.
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            if has_handle {
                "handle and nodeID are mutually exclusive"
            } else {
                "handle or nodeID required"
            },
        ));
    }
    let node = if has_node {
        let node = geo_entity_node(w, doc, nid)?; // :2504
        if w.node_locked(node) {
            return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked")); // :2506
        }
        node
    } else {
        match handle_lookup(cstr(&htok)) {
            Ok(node) => node,
            Err(why) => return Err(OpErr::new(-32005, "StaleHandle", why)), // :2512
        }
    };
    let bytes = bytes.unwrap_or(b"");
    let Some(vp) = json_find_top(bytes, b"values") else {
        return Err(OpErr::new(-32602, "InvalidParams", "values")); // :2518
    };
    if byte_at(bytes, vp) != b'{' {
        return Err(OpErr::new(-32602, "InvalidParams", "values"));
    }
    let Some(element) = w.properties(node) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "entity_properties missing",
        )); // :2522
    };
    Ok(SetKvTarget { node, element, vp })
}

/// `entity.setKeyValues`'s arm half (`mcp_bridge.c:5813-5854`): the arm opens the undo group
/// (`:5839`). The C's `op_set_keyvalues` (`:2447-2452`) opens none, and `edit.batch` (`:5932`)
/// calls the same body inside its own group (`:5878`) — hence [`op_set_keyvalues_no_group`].
///
/// `record` is the C's `t->record`: 0 for a node an `edit.batch` created in the same group (its
/// creation already recorded it). The standalone arm always passes 1.
pub(crate) fn op_set_keyvalues(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
    record: bool,
) -> Result<bool, OpErr> {
    set_keyvalues_with(w, doc, params, out, record, Some(c"MCP: set keyvalues"))
}

/// `edit.batch`'s per-op `entity.setKeyValues` (`mcp_bridge.c:2447-2452`, called at `:5932`): the
/// same body WITHOUT an undo group — the batch opened ONE for the whole run (`:5878`).
pub(crate) fn op_set_keyvalues_no_group(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
    record: bool,
) -> Result<bool, OpErr> {
    set_keyvalues_with(w, doc, params, out, record, None)
}

/// `set_keyvalues_target` + `set_keyvalues_apply` (`mcp_bridge.c:2383-2445`). `group` is the C's
/// `undo_open` label: `Some` for the standalone arm (`:5839`), `None` for `edit.batch`, whose own
/// group is already open (`:5878`). The group opens only after the target validated (`:5834-5836`).
fn set_keyvalues_with(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
    record: bool,
    group: Option<&'static CStr>,
) -> Result<bool, OpErr> {
    let target = set_keyvalues_target(w, doc, params)?;
    if let Some(label) = group {
        w.undo_open(doc, label); // :5836
    }
    if record {
        // :2536: RecordNodeTree ONCE, before the first write
        let hist = w.get_active_history();
        w.record_node_tree(hist, target.node);
    }
    let bytes = params.map(str::as_bytes).unwrap_or(b"");
    let apply = apply_keyvalues_with(w, target.node, target.element, bytes, target.vp, true);
    let aux = kv_apply_into(out, &apply, true);
    w.doc_set_modified(doc); // :2540
    Ok(aux)
}

/// `op_entity_add_connections` (`mcp_bridge.c:3714-3811`).
pub(crate) fn op_entity_add_connections(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    };
    let bytes = params.as_bytes();
    if json_find_top(bytes, b"nodeID").is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required")); // :3716
    }
    let Some(id) = json_int_top(bytes, b"nodeID") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    if !(0..=NODEID_MAX).contains(&id) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        )); // :3720
    }
    let Some(cp) = json_find_top(bytes, b"connections") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "connections: [{output, target, input, param?, delay?, timesToFire?}] required",
        )); // :3725
    };
    if byte_at(bytes, cp) != b'[' {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "connections: [{output, target, input, param?, delay?, timesToFire?}] required",
        ));
    }
    // The C's `specs` is a `static ConnSpec specs[CONN_MAX]` (`:3724`); on the heap here, because
    // CONN_MAX * sizeof(ConnSpec) is ~66 KB and the op body runs on Hammer's main thread stack.
    // Same size, same parse, no wire change.
    let mut specs = vec![ConnSpec::default(); crate::entity_json::CONN_MAX];
    let Some(n) = crate::entity_json::conn_array_parse(
        &bytes[cp..],
        &mut specs,
        crate::entity_json::CONN_MAX,
    ) else {
        // :3732-3735: short on purpose (`data` is 512 bytes; the C's comment says 192).
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "connections: 1..64 entries of {output, target, input, param?, delay?, timesToFire?}",
        ));
    };
    let node = geo_entity_node(w, doc, id)?; // :3736
    if w.node_locked(node) {
        return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked")); // :3738
    }
    // :3742-3750: the baseline the read-back is checked against, and the cap, before the group.
    let Some(count0) = w.conn_count(node) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "connection vector unreadable",
        ));
    };
    if count0 + n > RM_INDEX_MAX {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "connections: this entity has room for fewer than the requested entries (max 512 live)",
        ));
    }
    let hist = w.undo_open(doc, c"MCP: add connections"); // :3752
    w.record_node_tree(hist, node);
    let mut results = Vec::with_capacity(n);
    let mut baseline = count0;
    for spec in &specs[..n] {
        let (landed, reported) = w.add_connection(node, spec); // :3755-3776
        let count1 = w.conn_count(node); // :3778
        let mut ok = landed && count1 == Some(baseline + 1) && reported == Some(baseline as i32);
        let conn = if ok {
            w.conn_entry(node, baseline) // :3781: conn_entry_ok + the field reads
        } else {
            None
        };
        ok = ok && conn.is_some();
        results.push(ConnAdd {
            ok,
            reported: if landed { reported } else { None },
            conn,
        });
        if let Some(count1) = count1 {
            baseline = count1; // :3797: resync from the vector itself
        }
    }
    w.doc_set_modified(doc); // :3800
    Ok(add_connections_into(out, id, &results))
}

/// `op_entity_remove_connections` (`mcp_bridge.c:3842-3990`).
pub(crate) fn op_entity_remove_connections(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    };
    let bytes = params.as_bytes();
    if json_find_top(bytes, b"nodeID").is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required")); // :3844
    }
    let Some(id) = json_int_top(bytes, b"nodeID") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    if !(0..=NODEID_MAX).contains(&id) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        )); // :3848
    }
    let ip = json_find_top(bytes, b"indices");
    let mp = json_find_top(bytes, b"match");
    if !crate::entity_json::rm_mode_ok(ip.is_some(), mp.is_some()) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "exactly one of indices / match is required",
        )); // :3853
    }
    let mut indices: Vec<i32> = Vec::new();
    let filter = match ip {
        Some(ip) => {
            let mut parsed = [0i32; RM_INDEX_MAX];
            let Some(n) = crate::entity_json::rm_indices_parse(
                &bytes[ip..],
                &mut parsed,
                RM_INDEX_MAX,
            ) else {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "indices: 1..512 distinct non-negative ints up to 2147483647",
                )); // :3862
            };
            indices.extend_from_slice(&parsed[..n]);
            None
        }
        None => {
            // `rm_mode_ok` above proved `match` is the member present here.
            let mp = mp.expect("rm_mode_ok proved `match` is present");
            if byte_at(bytes, mp) != b'{' {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: {output?, target?, input?} required",
                )); // :3865
            }
            let mut filter = ConnFilter {
                output: None,
                target: None,
                input: None,
            };
            let mut p = mp + 1;
            loop {
                let before = p;
                let mut name = [0u8; 64];
                let mut sval = vec![0u8; crate::entity_json::CONN_STR_MAX + 1];
                let mut nval = [0u8; KV_NAME_CAP];
                let Some(value) =
                    crate::entity_json::kv_next(bytes, p, &mut name, &mut sval, &mut nval)
                else {
                    return Err(OpErr::new(-32602, "InvalidParams", "match: malformed")); // :3876
                };
                p = value.next;
                if p == before {
                    return Err(OpErr::new(-32602, "InvalidParams", "match: malformed"));
                }
                if value.over != 0
                    || value.kind != crate::entity_json::KV_STRING
                    || !crate::entity_json::conn_text_ok(&sval, true)
                {
                    return Err(OpErr::new(
                        -32602,
                        "InvalidParams",
                        "match: output/target/input are 1..255 bytes without control characters",
                    )); // :3878-3880
                }
                if seq(&name, b"output") && filter.output.is_none() {
                    filter.output = Some(cstr(&sval).to_vec());
                } else if seq(&name, b"target") && filter.target.is_none() {
                    filter.target = Some(cstr(&sval).to_vec());
                } else if seq(&name, b"input") && filter.input.is_none() {
                    filter.input = Some(cstr(&sval).to_vec());
                } else {
                    // :3886-3888
                    return Err(OpErr::new(
                        -32602,
                        "InvalidParams",
                        "match: only output, target and input",
                    ));
                }
                p = jws(bytes, p);
                if byte_at(bytes, p) == b',' {
                    p += 1;
                    continue;
                }
                if byte_at(bytes, p) == b'}' {
                    break;
                }
                return Err(OpErr::new(-32602, "InvalidParams", "match: malformed"));
            }
            if filter.output.is_none() && filter.target.is_none() && filter.input.is_none() {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: at least one of output, target, input is required",
                )); // :3898
            }
            Some(filter)
        }
    };
    let node = geo_entity_node(w, doc, id)?; // :3900
    if w.node_locked(node) {
        return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked")); // :3902
    }
    let Some(count) = w.conn_count(node) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "connection vector unreadable",
        )); // :3906
    };
    match filter {
        None => {
            // :3908-3920: every index must be in range before the group opens.
            for &index in &indices {
                if index as usize >= count {
                    let mut why = vec![0u8; 128];
                    let mut b = Sb::new(&mut why);
                    b.add(b"indices: ");
                    b.addi(i64::from(index));
                    b.add(b" is out of range (the entity has ");
                    b.addi(count as i64);
                    b.add(b" connection(s))");
                    let text = String::from_utf8_lossy(cstr(&why)).into_owned();
                    return Err(OpErr::new(-32602, "InvalidParams", &text));
                }
            }
        }
        Some(filter) => {
            // :3921-3958: resolve the filter against the live vector.
            for index in 0..count {
                let Some(fields) = w.conn_filter_fields(node, index) else {
                    continue; // an unreadable entry never matches
                };
                let matches = filter
                    .output
                    .as_ref()
                    .is_none_or(|want| fields.output.as_ref().is_some_and(|got| seq(got, want)))
                    && filter
                        .target
                        .as_ref()
                        .is_none_or(|want| fields.target.as_ref().is_some_and(|got| seq(got, want)))
                    && filter
                        .input
                        .as_ref()
                        .is_none_or(|want| fields.input.as_ref().is_some_and(|got| seq(got, want)));
                if !matches {
                    continue;
                }
                if indices.len() >= RM_INDEX_MAX {
                    return Err(OpErr::new(
                        -32602,
                        "InvalidParams",
                        "match: more than 512 connections match",
                    )); // :3949
                }
                indices.push(index as i32);
            }
            if indices.is_empty() {
                return Err(OpErr::new(
                    -32026,
                    "ConnectionNotFound",
                    "no live connection matches the match filter",
                )); // :3954
            }
        }
    }
    crate::entity_json::rm_sort_desc(&mut indices); // :3958: highest first
    let hist = w.undo_open(doc, c"MCP: remove connections"); // :3959
    w.record_node_tree(hist, node);
    for &index in &indices {
        if !w.remove_connection(node, index) {
            // :3961-3966: the earlier removals in this group already ran.
            return Err(OpErr::maybe(
                -32603,
                "InternalError",
                "RemoveConnection refused an index; the removals before it in this group already ran - verify with entity.get, do not retry",
            ));
        }
    }
    w.doc_set_modified(doc); // :3968
    let count = count as i64;
    remove_connections_into(out, id, &indices, count);
    Ok(false)
}

/// `op_node_delete` (`mcp_bridge.c:5475-5511`).
pub(crate) fn op_node_delete(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    let bytes = params.map(str::as_bytes).unwrap_or(b"");
    let Some(mut p) = json_find_top(bytes, b"nodeIDs") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeIDs: [int,...] required",
        )); // :5477
    };
    if byte_at(bytes, p) != b'[' {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeIDs: [int,...] required",
        ));
    }
    let mut ids: Vec<i32> = Vec::new();
    p = jws(bytes, p + 1); // :5479
    while byte_at(bytes, p) != b']' {
        let Some((next, value)) = jnum(bytes, p) else {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "nodeIDs: up to 256 non-negative ints",
            )); // :5487
        };
        p = next;
        // :5485-5489: the whole id must be an integer in the i32 range (a bigger one would alias
        // another node through the C's `(int)` cast).
        if !(0.0..=2147483647.0).contains(&value)
            || value != f64::from(value as i32)
            || ids.len() >= 256
        {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "nodeIDs: up to 256 non-negative ints",
            ));
        }
        ids.push(value as i32);
        p = jws(bytes, p);
        if byte_at(bytes, p) == b',' {
            p = jws(bytes, p + 1);
        } else if byte_at(bytes, p) != b']' {
            return Err(OpErr::new(-32602, "InvalidParams", "nodeIDs: malformed")); // :5491
        }
    }
    if ids.is_empty() {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeIDs: empty")); // :5493
    }
    let mut nodes = Vec::with_capacity(ids.len());
    for &id in &ids {
        let Some(node) = geo_find(w, doc, id) else {
            let mut why = vec![0u8; 96];
            let mut b = Sb::new(&mut why);
            b.add(b"nodeID ");
            b.addi(i64::from(id));
            b.add(b" not found");
            let text = String::from_utf8_lossy(cstr(&why)).into_owned();
            return Err(OpErr::new(-32022, "NodeNotFound", &text)); // :5498-5500
        };
        let class = w.class(node);
        if class == "world" || class == "other" {
            let mut why = vec![0u8; 96];
            let mut b = Sb::new(&mut why);
            b.add(b"nodeID ");
            b.addi(i64::from(id));
            b.add(b" cannot be deleted (");
            b.add(class.as_bytes());
            b.add(b")");
            let text = String::from_utf8_lossy(cstr(&why)).into_owned();
            return Err(OpErr::new(-32602, "InvalidParams", &text)); // :5501-5503
        }
        if w.node_locked(node) {
            let mut why = vec![0u8; 96];
            let mut b = Sb::new(&mut why);
            b.add(b"nodeID ");
            b.addi(i64::from(id));
            b.add(b" is locked");
            let text = String::from_utf8_lossy(cstr(&why)).into_owned();
            return Err(OpErr::new(-32018, "DocumentNotEditable", &text)); // :5504
        }
        nodes.push(node);
    }
    w.undo_open(doc, c"MCP: delete nodes"); // :5506
    for node in &nodes {
        w.delete_node(doc, *node);
    }
    node_delete_into(out, &ids);
    Ok(false)
}

/// `op_node_set_transform` (`mcp_bridge.c:5539-5610`).
pub(crate) fn op_node_set_transform(
    w: &dyn Write,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<bool, OpErr> {
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    };
    let bytes = params.as_bytes();
    if json_find_top(bytes, b"nodeID").is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required")); // :5541
    }
    let Some(id) = json_int_top(bytes, b"nodeID") else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    if !(0..=NODEID_MAX).contains(&id) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        )); // :5545
    }
    let origin = json_float3_top(bytes, b"origin");
    let angles = json_float3_top(bytes, b"angles");
    let scales = json_float3_top(bytes, b"scales");
    if json_find_top(bytes, b"origin").is_some() && origin.is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "origin: [x,y,z] required")); // :5550
    }
    if json_find_top(bytes, b"angles").is_some() && angles.is_none() {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "angles: [pitch,yaw,roll] required",
        )); // :5554
    }
    if json_find_top(bytes, b"scales").is_some() && scales.is_none() {
        return Err(OpErr::new(-32602, "InvalidParams", "scales: [x,y,z] required")); // :5558
    }
    if !xform_any(origin.is_some(), angles.is_some(), scales.is_some()) {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "at least one of origin, angles, scales is required",
        )); // :5562
    }
    if let Some(origin) = origin {
        if !xform_origin_ok(&origin) {
            let mut why = vec![0u8; 96];
            xform_origin_reason(&mut why);
            let text = String::from_utf8_lossy(cstr(&why)).into_owned();
            return Err(OpErr::new(-32602, "InvalidParams", &text)); // :5568
        }
    }
    if let Some(scales) = scales {
        if !xform_scales_ok(&scales) {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "scales: each finite and in (0, 1000]",
            )); // :5572
        }
    }
    let Some(node) = geo_find(w, doc, id as i32) else {
        return Err(OpErr::new(
            -32022,
            "NodeNotFound",
            "no node with this nodeID in the live document",
        )); // :5579
    };
    // :5580-5583: the C's two obj_is checks, through the same class predicate node_class uses.
    if !w.is_of(node.0, pins::HV_MAPENTITY_VT.value as u32)
        && !w.is_of(node.0, pins::HV_MAPMESH_VT.value as u32)
    {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "not an entity or mesh",
        ));
    }
    if w.node_locked(node) {
        return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked")); // :5584
    }
    let hist = w.undo_open(doc, c"MCP: set transform"); // :5585
    w.record_node_tree(hist, node);
    // :5588-5591: Hammer's own order - scales, angles, origin.
    if let Some(scales) = scales {
        w.set_scales(node, scales);
    }
    if let Some(angles) = angles {
        w.set_angles(node, angles);
    }
    if let Some(origin) = origin {
        w.set_origin(node, origin);
    }
    w.doc_set_modified(doc); // :5593
    // :5594-5597: the three fields are read back, then the class; an unreadable one is `null`.
    let origin = w.origin(node);
    let angles = w.angles(node);
    let scales = w.scales(node);
    let class = w.class(node);
    set_transform_into(out, id, class, origin, angles, scales);
    Ok(false)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// The C's `geo_dispatch` head (`mcp_bridge.c:5719-5746`) for a read: resolve the live document,
/// then run the op. `mutating` is always 0 for this module's rows.
fn geo_read(
    hammer: &dyn Hammer,
    params: Option<&str>,
    out: &mut Sb,
    op: impl FnOnce(&dyn Hammer, Doc, Option<&str>, &mut Sb) -> Result<(), OpErr>,
) -> Result<(), OpErr> {
    let doc = hammer
        .active_doc()
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?;
    op(hammer, doc, params, out)
}

/// `entity.get`'s fully built reply (`geo_dispatch`, `:5747-5788`).
fn entity_get(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let outcome = geo_read(
        game(),
        req.params.as_deref(),
        &mut out,
        |hammer, doc, params, out| op_entity_get(hammer, doc, params, out),
    );
    finish(
        &req.id,
        outcome,
        &out,
        "entity has too many or too large keyvalues", // :5775
    )
}

/// `entity.find`'s fully built reply.
fn entity_find(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let outcome = geo_read(
        game(),
        req.params.as_deref(),
        &mut out,
        |hammer, doc, params, out| op_entity_find(hammer, doc, params, out),
    );
    finish(
        &req.id,
        outcome,
        &out,
        "response too large (use limit)", // :5777
    )
}

/// `node.list`'s fully built reply.
fn node_list(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let outcome = geo_read(
        game(),
        req.params.as_deref(),
        &mut out,
        |hammer, doc, params, out| op_node_list(hammer, doc, params, out),
    );
    finish(
        &req.id,
        outcome,
        &out,
        "response too large (use limit)", // :5777
    )
}

// ---------------------------------------------------------------------------------------------
// The write handlers (`mcp_bridge.c:5705-5860`, `:5976-5991` and `geo_dispatch`'s write head)
// ---------------------------------------------------------------------------------------------

/// An [`OpErr`] as the C's `build_error_ext` frame, for the arms that build no result.
pub(crate) fn error_reply(id: &str, error: &OpErr) -> String {
    if error.data.is_empty() {
        control::error(Some(id), error.code, error.name)
    } else {
        control::error_ext(Some(id), error.code, error.name, &error.data)
    }
}

/// The C's dispatch-arm tail for a mutating op (`geo_dispatch`, `:5660-5679`): splice the
/// document state in, then either the C's overflow error or the result envelope. `aux` is a
/// scratch-builder overflow the op reported before its own result was complete - the C's
/// `out->ovf = 1` (`:2417`, `:3797`), which the same tail turns into the same error.
pub(crate) fn write_finish(
    id: &str,
    outcome: Result<(Doc, bool), OpErr>,
    out: &mut Sb,
    overflow: &str,
) -> String {
    match outcome {
        Ok((doc, aux)) => {
            doc_tail_into(out, Some(doc));
            if aux {
                control::error(Some(id), -32603, overflow)
            } else {
                finish(id, Ok(()), out, overflow)
            }
        }
        Err(error) => finish(id, Err(error), out, overflow),
    }
}

/// `entity.create`'s arm (`mcp_bridge.c:5720-5751`): 16384-byte result buffer, its own overflow
/// text, and the document tail before the overflow check.
fn entity_create(req: &Request) -> String {
    let mut buf = vec![0u8; 16384];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_entity_create(w, doc, params, &mut out).map(|aux| (doc, aux)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use fewer keyvalues)", // :5744
    )
}

/// `entity.setKeyValues`'s arm (`mcp_bridge.c:5813-5854`): 65536-byte result buffer.
fn entity_set_key_values(req: &Request) -> String {
    let mut buf = vec![0u8; 65536];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes)).and_then(|doc| {
        op_set_keyvalues(w, doc, params, &mut out, true).map(|aux| (doc, aux))
    });
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use fewer keyvalues)", // :5848
    )
}

/// `entity.addConnections` through `geo_dispatch` (`:5980`).
fn entity_add_connections(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_entity_add_connections(w, doc, params, &mut out).map(|aux| (doc, aux)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)", // :5777
    )
}

/// `entity.removeConnections` through `geo_dispatch` (`:5982`).
fn entity_remove_connections(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes)).and_then(|doc| {
        op_entity_remove_connections(w, doc, params, &mut out).map(|aux| (doc, aux))
    });
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)",
    )
}

/// `node.delete` through `geo_dispatch` (`:5978`).
fn node_delete(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_node_delete(w, doc, params, &mut out).map(|aux| (doc, aux)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)",
    )
}

/// `node.setTransform` through `geo_dispatch` (`:5984`).
fn node_set_transform(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let params = req.params.as_deref();
    let outcome = write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_node_set_transform(w, doc, params, &mut out).map(|aux| (doc, aux)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)",
    )
}

/// One `undo.undo` / `undo.redo` run's answer, before the reply is rendered.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UndoStep {
    /// The popped group's label (`history_top_info`'s top name, read before the pop).
    pub name: Vec<u8>,
    /// `depth1 < depth0`: the stack actually shrank.
    pub undone: bool,
    /// The depth after the call (`undoDepth` / `redoDepth`).
    pub depth: u32,
}

/// The undo arm's body after the gate (`mcp_bridge.c:5757-5791`): the redo partner's class
/// check, the stack choice, the empty-stack answer and the pop.
///
/// `Ok(None)` is the C's `updated:false` result with an empty stack; `Err` is either the
/// `bad_redo_history` `-32017` or the gate's own error.
pub(crate) fn undo_step(
    w: &dyn Write,
    doc: Doc,
    redo: bool,
) -> Result<Option<UndoStep>, OpErr> {
    if redo {
        // :5773-5774: the pair partner must be readable (and its class right) before slot 2. The C
        // spells the guard `!GetRedoHistory(doc) || !obj_is(GetRedoHistory(doc), ...)`, so it makes
        // two calls when the first one is non-null and one when it is null - ported as written.
        if w.get_redo_history(doc) == 0 || !w.history_ok(w.get_redo_history(doc)) {
            return Err(OpErr::with_data(-32017, "PrerequisiteFailed", |db| {
                db.add(b"{\"reason\":\"bad_redo_history\"}");
            }));
        }
    }
    // :5782: undo acts on the ACTIVE history, redo on the redo stack (a third `GetRedoHistory`).
    let hist = if redo {
        w.get_redo_history(doc)
    } else {
        w.get_active_history()
    };
    let before = w.history_info(hist);
    if before.depth == 0 {
        // :5768-5780: an empty stack is a RESULT, never an error.
        return Ok(None);
    }
    w.undo_pop(hist); // :5781
    let after = w.history_info(hist);
    log::line(&format!(
        "{} {}",
        if redo { "redo: depth" } else { "undo: depth" },
        before.depth
    )); // :5784-5785
    log::line(&format!(
        "{} {}",
        if redo { "redo: ->" } else { "undo: ->" },
        after.depth
    ));
    Ok(Some(UndoStep {
        name: before.name,
        undone: after.depth < before.depth, // :5788
        depth: after.depth,
    }))
}

/// `undo.undo` / `undo.redo` (`mcp_bridge.c:5747-5812`): the reply is built straight into the
/// envelope, with the document object inside it (`sb_add_doc_object(&b, doc)`, `:5771`).
fn undo_op(req: &Request, redo: bool) -> String {
    let w = game_write();
    let params = req.params.as_deref();
    let doc = match write_gate(w, params.map(str::as_bytes)) {
        Ok(doc) => doc,
        Err(error) => return error_reply(&req.id, &error),
    };
    let step = match undo_step(w, doc, redo) {
        Ok(step) => step,
        Err(error) => return error_reply(&req.id, &error),
    };
    let mut docbuf = vec![0u8; 512];
    let mut doc_object = Sb::new(&mut docbuf);
    sb_add_doc_object(&mut doc_object, doc); // :5771
    let doc_object = String::from_utf8_lossy(doc_object.as_cstr()).into_owned();
    let popped = step
        .as_ref()
        .map(|step| (step.name.as_slice(), step.undone, step.depth));
    undo_reply(&req.id, redo, popped, &doc_object)
}

/// `undo.undo`'s arm.
fn undo_undo(req: &Request) -> String {
    undo_op(req, false)
}

/// `undo.redo`'s arm.
fn undo_redo(req: &Request) -> String {
    undo_op(req, true)
}

/// This group's bodies. One line per method in the C's dispatch chain (`:5705-5985`).
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("entity.create", entity_create),
    ("undo.undo", undo_undo),
    ("undo.redo", undo_redo),
    ("entity.setKeyValues", entity_set_key_values),
    ("node.delete", node_delete),
    ("node.list", node_list),
    ("node.setTransform", node_set_transform),
    ("entity.addConnections", entity_add_connections),
    ("entity.removeConnections", entity_remove_connections),
    ("entity.get", entity_get),
    ("entity.find", entity_find),
    ("doc.active", doc_active),
];

// The two dirty gates sit outside `bind` so the ABI test builds off Windows too. Both return
// a `bool` in AL; the game leaves the upper bits of EAX undefined (a heap pointer, the 64-bit
// last-saved mark), so the return is `u8` and only AL is read.
#[cfg(any(windows, test))]
pub(crate) type FnDocOrDependencyChanged = unsafe extern "C" fn(*mut c_void, i32) -> u8;
// Returns a `bool` in AL, upper bits undefined: `u8`, see `FnDocOrDependencyChanged`.
#[cfg(any(windows, test))]
pub(crate) type FnChangedSinceSave = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8;

// ---------------------------------------------------------------------------------------------
// The `g_hfn` bind table (`mcp_bridge.c:1642-1691`, signatures `s2sdk.h:698-742`)
// ---------------------------------------------------------------------------------------------

/// The bind table exists only on the Windows tools build: its entries are hammer.dll's
/// functions, and nothing else ever loads that module.
#[cfg(windows)]
pub(crate) mod bind {
    use core::ffi::{c_char, c_void};
    use std::sync::Mutex;

    use s2sdk_sys::generated::CUtlSymbolLarge;

    use crate::modules;

    use super::{lock, pins, FnChangedSinceSave, FnDocOrDependencyChanged};

// The C's `HammerFns`: functions hammer.dll does not export and no interface exposes, bound
// once from the generated `HFN_*` RVAs after the module identity check. Only a read's entries
// are called by this lane; the whole table is bound for the write lanes that build on it.
type FnGetUndoHistory = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnGetRedoHistory = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnGetActiveHistory = unsafe extern "C" fn() -> *mut c_void;
type FnAddOperation =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, i64);
type FnRecord = unsafe extern "C" fn(*mut c_void, *mut c_void, i32);
type FnRecordNodesAndParents = unsafe extern "C" fn(*mut c_void, *const c_void, i32);
type FnDocMoveToWorld = unsafe extern "C" fn(*mut c_void, *const c_void);
type FnActivateDoc = unsafe extern "C" fn(*mut c_void, i32);
type FnDocSetModified = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FnFindTypeByName = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type FnCreateNodeByName =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut f32, *mut f32) -> *mut c_void;
type FnInsertNode = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type FnDeleteNode = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FnNodeIsLocked = unsafe extern "C" fn(*mut c_void, i32) -> u8;
type FnGetEntityProperties = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnFindAttribute = unsafe extern "C" fn(*mut c_void, *const CUtlSymbolLarge) -> *mut c_void;
type FnCreateAttribute =
    unsafe extern "C" fn(*mut c_void, *const CUtlSymbolLarge, u32) -> *mut c_void;
type FnSetValueString = unsafe extern "C" fn(*mut c_void, *const CUtlSymbolLarge);
type FnSetValueInt = unsafe extern "C" fn(*mut c_void, *const i32);
type FnSetValueFloat = unsafe extern "C" fn(*mut c_void, *const f32);
type FnSetKeyValue = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);
type FnAddConnection = unsafe extern "C" fn(*mut c_void, *mut u32, *const c_void) -> *mut c_void;
type FnRemoveConnection = unsafe extern "C" fn(*mut c_void, u32) -> u8;
type FnKv3Init = unsafe extern "C" fn(*mut c_void, u8, u8) -> *mut c_void;
type FnKv3Free = unsafe extern "C" fn(*mut c_void);
type FnCreateMeshElement =
    unsafe extern "C" fn(u32, u64, *const c_char, i32) -> *mut c_void;
type FnMeshInitFromPolygons =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char, i32, *mut c_void);
type FnAssignMaterialToFaces =
    unsafe extern "C" fn(*mut c_void, *const c_void, i32, *const c_char);
type FnCollectHandles = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8;
type FnMeshDeleteFaces = unsafe extern "C" fn(*mut c_void, *mut c_void);
/// `mesh.get`'s handle accessors: `(poly, out*, const handle*) -> out*`.
type FnMeshHandleOp = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> *mut c_void;
/// `(poly, const MeshHandle *verts, int n, const matrix3x4_t *xf, Vector *out) -> bool`.
type FnMeshVertexPositions =
    unsafe extern "C" fn(*mut c_void, *const c_void, i32, *const c_void, *mut c_void) -> u8;
type FnMeshFaceMaterialIndex = unsafe extern "C" fn(*mut c_void, *const c_void) -> i32;
type FnMeshMaterialName = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
/// `HFN_MESH_SET_FACE_TEXTURE`: `(poly, HandleVec*, set, const Vec4 *U, const Vec4 *V, const Vec2 *scale)`.
type FnMeshSetFaceTexture =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *const f32, *const f32, *const f32);
/// `HFN_MESH_GET_FACE_TEXTURE`: `(poly, const MeshHandle*, set, Vec4 *U, Vec4 *V, Vec2 *scale)`.
type FnMeshGetFaceTexture =
    unsafe extern "C" fn(*mut c_void, *const c_void, u32, *mut f32, *mut f32, *mut f32);
/// `HFN_MESH_JUSTIFY_FACE_TEXTURE`: `(CMapMesh*, HandleVec*, set, mode, const void *bounds = NULL)`.
type FnMeshJustifyFaceTexture =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, *const c_void);
/// `HFN_MESH_ALIGN_FACE_TEXTURE`: `(CMapMesh*, HandleVec*, set, mode, flags, const float origin[12])`.
type FnMeshAlignFaceTexture =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, i32, i32, *const f32);
type FnGetPreferred3DView = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnSessionUpdateViews = unsafe extern "C" fn(*mut c_void);
type FnRenderViewToMemory = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    i32,
    i32,
    i32,
    *mut c_void,
    *mut c_void,
) -> u64;
type FnFrustum = unsafe extern "C" fn(*mut c_void);
type FnCameraGet = unsafe extern "C" fn(*mut c_void, *mut f32);
type FnToggleToolsMaterials = unsafe extern "C" fn(*mut c_void);
// The selection functions (`ops::selection`, bridge m3.12); every bool is a byte, every pointer
// an opaque Hammer object or a caller-owned `CUtlVector`.
// The prefab workers (`ops::prefab`, bridge m3.18; RE notes `re-prefab.md`).
/// `HFN_DOC_ADD_PREFAB`: `(CMapDoc*, const char *absPath) -> CMapPrefab*` (null: not created).
type FnDocAddPrefab = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
/// `HFN_DOC_PREFAB_FROM_SELECTION`: `(CMapDoc*, const char *absPath, const Frame*, bool centered)
/// -> CMapPrefab*`.
type FnDocPrefabFromSelection =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, u8) -> *mut c_void;
/// `HFN_PREFAB_GET_TARGET_MAP`: `(CMapPrefab*) -> const char *` (the stored relative path, or a
/// static empty string).
type FnPrefabGetTargetMap = unsafe extern "C" fn(*mut c_void) -> *const c_char;
/// `HFN_PREFAB_GET_WORLD`: `(CMapPrefab*) -> CMapNode*` (the loaded world, null when not loaded).
type FnPrefabGetWorld = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnSelCurrentSet = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnSelGetSet = unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void;
type FnSelCount = unsafe extern "C" fn(*mut c_void) -> i32;
type FnSelOut = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FnSelSelectNodes = unsafe extern "C" fn(*mut c_void, *const c_void, i32, u8);
/// `(sel, mode, convert)`: the third argument is the conversion kind; the bridge always passes 0.
type FnSelSetMode = unsafe extern "C" fn(*mut c_void, i32, i32);
type FnFaceSelGetFaces = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type FnFaceSelSelectFaces = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void, i32, u8);

/// The C's `HammerFns` (`s2sdk.h:698-742`), field for field.
///
/// The whole table is bound even where no read calls an entry: the write lanes (`entity.*`,
/// `mesh.*`, `undo.*`, `edit.batch`) call the rest, and binding is one pass over the pins.
#[allow(dead_code)] // entries the read lane does not call belong to the next write lanes
#[derive(Copy, Clone)]
pub(crate) struct HammerFns {
    pub(super) get_undo_history: FnGetUndoHistory,
    pub(super) get_redo_history: FnGetRedoHistory,
    pub(crate) get_active_history: FnGetActiveHistory,
    pub(crate) add_operation: FnAddOperation,
    pub(crate) record_node_tree: FnRecord,
    pub(crate) record_node: FnRecord,
    /// `HFN_RECORD_NODES_AND_PARENTS`: the m2.19 tie's reparent recording (the EDIT lane).
    pub(crate) record_nodes_and_parents: FnRecordNodesAndParents,
    /// `HFN_DOC_MOVE_TO_WORLD`: the m2.19 untie primitive (the EDIT lane).
    pub(crate) doc_move_to_world: FnDocMoveToWorld,
    pub(super) activate_doc: FnActivateDoc,
    pub(crate) doc_or_dependency_changed: FnDocOrDependencyChanged,
    pub(super) changed_since_save: FnChangedSinceSave,
    pub(super) doc_set_modified: FnDocSetModified,
    pub(super) find_type_by_name: FnFindTypeByName,
    pub(super) create_node_by_name: FnCreateNodeByName,
    pub(crate) insert_node: FnInsertNode,
    pub(super) delete_node: FnDeleteNode,
    pub(super) node_is_locked: FnNodeIsLocked,
    pub(super) get_entity_properties: FnGetEntityProperties,
    pub(super) find_attribute: FnFindAttribute,
    pub(super) create_attribute: FnCreateAttribute,
    pub(super) set_value_string: FnSetValueString,
    pub(super) set_value_int: FnSetValueInt,
    pub(super) set_value_float: FnSetValueFloat,
    pub(super) set_key_value: FnSetKeyValue,
    pub(super) add_connection: FnAddConnection,
    pub(super) remove_connection: FnRemoveConnection,
    /// `HFN_KV3_INIT`: the connection `paramMap` initializer (HAMMER-W1's addConnections).
    pub(super) kv3_init: FnKv3Init,
    /// `HFN_KV3_FREE`: pairs `kv3_init`.
    pub(super) kv3_free: FnKv3Free,
    pub(crate) create_mesh_element: FnCreateMeshElement,
    pub(crate) mesh_init_from_polygons: FnMeshInitFromPolygons,
    pub(crate) assign_material_to_faces: FnAssignMaterialToFaces,
    pub(crate) collect_handles: FnCollectHandles,
    pub(crate) mesh_delete_faces: FnMeshDeleteFaces,
    /// `mesh.get`'s read pins (re-report R1 + FOLLOW-UP): face first edge, edge next, edge vertex,
    /// the checked batch vertex positions. (`HFN_MESH_EDGE_POSITIONS` is a pin only: unsafe.)
    pub(crate) mesh_face_first_edge: FnMeshHandleOp,
    pub(crate) mesh_edge_next: FnMeshHandleOp,
    pub(crate) mesh_edge_vertex: FnMeshHandleOp,
    pub(crate) mesh_vertex_positions: FnMeshVertexPositions,
    pub(crate) mesh_face_material_index: FnMeshFaceMaterialIndex,
    pub(crate) mesh_material_name: FnMeshMaterialName,
    /// `mesh.setFaceTexture`'s four pins (WP1.5, `target/orch/wp15-re.md`).
    pub(crate) mesh_set_face_texture: FnMeshSetFaceTexture,
    pub(crate) mesh_get_face_texture: FnMeshGetFaceTexture,
    pub(crate) mesh_justify_face_texture: FnMeshJustifyFaceTexture,
    pub(crate) mesh_align_face_texture: FnMeshAlignFaceTexture,
    pub(crate) get_preferred_3d_view: FnGetPreferred3DView,
    pub(crate) session_update_views: FnSessionUpdateViews,
    pub(crate) render_view_to_memory: FnRenderViewToMemory,
    pub(crate) frustum_init: FnFrustum,
    pub(crate) frustum_update: FnFrustum,
    pub(crate) camera_get_position: FnCameraGet,
    pub(crate) camera_get_angles: FnCameraGet,
    pub(crate) toggle_tools_materials: FnToggleToolsMaterials,
    /// `HFN_SEL_CURRENT_SET` .. `HFN_FACESEL_SELECT_FACES`: the selection lane (`ops::selection`).
    pub(crate) sel_current_set: FnSelCurrentSet,
    pub(crate) sel_get_set: FnSelGetSet,
    pub(crate) sel_count: FnSelCount,
    pub(crate) sel_get_nodes: FnSelOut,
    pub(crate) sel_select_nodes: FnSelSelectNodes,
    pub(crate) sel_set_mode: FnSelSetMode,
    pub(crate) facesel_get_meshes: FnSelOut,
    pub(crate) facesel_get_faces: FnFaceSelGetFaces,
    pub(crate) facesel_select_faces: FnFaceSelSelectFaces,
    pub(crate) doc_add_prefab: FnDocAddPrefab,
    pub(crate) doc_prefab_from_selection: FnDocPrefabFromSelection,
    pub(crate) prefab_get_target_map: FnPrefabGetTargetMap,
    pub(crate) prefab_get_world: FnPrefabGetWorld,
}

/// C `g_hfn` + `g_hfnBound` (`mcp_bridge.c:1616-1619`): bound once, on the main thread.
static HFN: Mutex<Option<HammerFns>> = Mutex::new(None);

/// `hammer_bind` (`mcp_bridge.c:1642-1691`): bind every entry from `module base + HFN_RVA`.
///
/// The bound flag is set only after the whole table was written (a second call is a no-op),
/// exactly like `g_hfnBound`. The module gate ran before any caller gets here (`active_doc`
/// requires `mod_ok(PIN_HAMMER)`), and the prologue pins of these functions were re-checked
/// by the module verifier.
pub(super) fn hammer_bind() {
    if lock(&HFN).is_some() {
        return;
    }
    if !modules::live().is_verified(pins::PIN_HAMMER) {
        return;
    }
    let hb = modules::live().base(pins::PIN_HAMMER) as usize;
    macro_rules! bind {
        ($pin:ident, $ty:ty) => {{
            let address = hb + pins::$pin.value as usize;
            // SAFETY: `address` is a verified module's image at the pinned function RVA (the
            // module gate re-checked the prologue bytes) and `$ty` is the C's own signature
            // from `s2sdk.h`. The C writes the same address into the same typed slot
            // (`:1645`).
            unsafe { core::mem::transmute::<usize, $ty>(address) }
        }};
    }
    let table = HammerFns {
        get_undo_history: bind!(HFN_GET_UNDO_HISTORY, FnGetUndoHistory),
        get_redo_history: bind!(HFN_GET_REDO_HISTORY, FnGetRedoHistory),
        get_active_history: bind!(HFN_GET_ACTIVE_HISTORY, FnGetActiveHistory),
        add_operation: bind!(HFN_ADD_OPERATION, FnAddOperation),
        record_node_tree: bind!(HFN_RECORD_NODE_TREE, FnRecord),
        record_node: bind!(HFN_RECORD_NODE, FnRecord),
        record_nodes_and_parents: bind!(HFN_RECORD_NODES_AND_PARENTS, FnRecordNodesAndParents),
        doc_move_to_world: bind!(HFN_DOC_MOVE_TO_WORLD, FnDocMoveToWorld),
        activate_doc: bind!(HFN_ACTIVATE_DOC, FnActivateDoc),
        doc_or_dependency_changed: bind!(HFN_DOC_OR_DEPENDENCY_CHANGED, FnDocOrDependencyChanged),
        changed_since_save: bind!(HFN_CHANGED_SINCE_SAVE, FnChangedSinceSave),
        doc_set_modified: bind!(HFN_DOC_SET_MODIFIED, FnDocSetModified),
        find_type_by_name: bind!(HFN_FIND_TYPE_BY_NAME, FnFindTypeByName),
        create_node_by_name: bind!(HFN_CREATE_NODE_BY_NAME, FnCreateNodeByName),
        insert_node: bind!(HFN_INSERT_NODE, FnInsertNode),
        delete_node: bind!(HFN_DELETE_NODE, FnDeleteNode),
        node_is_locked: bind!(HFN_NODE_IS_LOCKED, FnNodeIsLocked),
        get_entity_properties: bind!(HFN_GET_ENTITY_PROPERTIES, FnGetEntityProperties),
        find_attribute: bind!(HFN_FIND_ATTRIBUTE, FnFindAttribute),
        create_attribute: bind!(HFN_CREATE_ATTRIBUTE, FnCreateAttribute),
        set_value_string: bind!(HFN_SET_STRING, FnSetValueString),
        set_value_int: bind!(HFN_SET_INT, FnSetValueInt),
        set_value_float: bind!(HFN_SET_FLOAT, FnSetValueFloat),
        set_key_value: bind!(HFN_SET_KEY_VALUE, FnSetKeyValue),
        add_connection: bind!(HFN_ADD_CONNECTION, FnAddConnection),
        remove_connection: bind!(HFN_REMOVE_CONNECTION, FnRemoveConnection),
        kv3_init: bind!(HFN_KV3_INIT, FnKv3Init),
        kv3_free: bind!(HFN_KV3_FREE, FnKv3Free),
        create_mesh_element: bind!(HFN_CREATE_MESH_ELEMENT, FnCreateMeshElement),
        mesh_init_from_polygons: bind!(HFN_MESH_INIT_FROM_POLYGONS, FnMeshInitFromPolygons),
        assign_material_to_faces: bind!(HFN_ASSIGN_MATERIAL_TO_FACES, FnAssignMaterialToFaces),
        collect_handles: bind!(HFN_COLLECT_HANDLES, FnCollectHandles),
        mesh_delete_faces: bind!(HFN_MESH_DELETE_FACES, FnMeshDeleteFaces),
        mesh_face_first_edge: bind!(HFN_MESH_FACE_FIRST_EDGE, FnMeshHandleOp),
        mesh_edge_next: bind!(HFN_MESH_EDGE_NEXT, FnMeshHandleOp),
        mesh_edge_vertex: bind!(HFN_MESH_EDGE_VERTEX, FnMeshHandleOp),
        mesh_vertex_positions: bind!(HFN_MESH_VERTEX_POSITIONS, FnMeshVertexPositions),
        mesh_face_material_index: bind!(HFN_MESH_FACE_MATERIAL_INDEX, FnMeshFaceMaterialIndex),
        mesh_material_name: bind!(HFN_MESH_MATERIAL_NAME, FnMeshMaterialName),
        mesh_set_face_texture: bind!(HFN_MESH_SET_FACE_TEXTURE, FnMeshSetFaceTexture),
        mesh_get_face_texture: bind!(HFN_MESH_GET_FACE_TEXTURE, FnMeshGetFaceTexture),
        mesh_justify_face_texture: bind!(HFN_MESH_JUSTIFY_FACE_TEXTURE, FnMeshJustifyFaceTexture),
        mesh_align_face_texture: bind!(HFN_MESH_ALIGN_FACE_TEXTURE, FnMeshAlignFaceTexture),
        get_preferred_3d_view: bind!(HFN_GET_PREFERRED_3D_VIEW, FnGetPreferred3DView),
        session_update_views: bind!(HFN_SESSION_UPDATE_VIEWS, FnSessionUpdateViews),
        render_view_to_memory: bind!(HFN_RENDER_VIEW_TO_MEMORY, FnRenderViewToMemory),
        frustum_init: bind!(HFN_FRUSTUM_INIT, FnFrustum),
        frustum_update: bind!(HFN_FRUSTUM_UPDATE, FnFrustum),
        camera_get_position: bind!(HFN_CAMERA_GET_POSITION, FnCameraGet),
        camera_get_angles: bind!(HFN_CAMERA_GET_ANGLES, FnCameraGet),
        toggle_tools_materials: bind!(HFN_TOGGLE_TOOLS_MATERIALS, FnToggleToolsMaterials),
        sel_current_set: bind!(HFN_SEL_CURRENT_SET, FnSelCurrentSet),
        sel_get_set: bind!(HFN_SEL_GET_SET, FnSelGetSet),
        sel_count: bind!(HFN_SEL_COUNT, FnSelCount),
        sel_get_nodes: bind!(HFN_SEL_GET_NODES, FnSelOut),
        sel_select_nodes: bind!(HFN_SEL_SELECT_NODES, FnSelSelectNodes),
        sel_set_mode: bind!(HFN_SEL_SET_MODE, FnSelSetMode),
        facesel_get_meshes: bind!(HFN_FACESEL_GET_MESHES, FnSelOut),
        facesel_get_faces: bind!(HFN_FACESEL_GET_FACES, FnFaceSelGetFaces),
        facesel_select_faces: bind!(HFN_FACESEL_SELECT_FACES, FnFaceSelSelectFaces),
        doc_add_prefab: bind!(HFN_DOC_ADD_PREFAB, FnDocAddPrefab),
        doc_prefab_from_selection: bind!(HFN_DOC_PREFAB_FROM_SELECTION, FnDocPrefabFromSelection),
        prefab_get_target_map: bind!(HFN_PREFAB_GET_TARGET_MAP, FnPrefabGetTargetMap),
        prefab_get_world: bind!(HFN_PREFAB_GET_WORLD, FnPrefabGetWorld),
    };
    *lock(&HFN) = Some(table); // :1688: g_hfnBound = 1
}

/// The bound table, or `None` before the first successful bind.
pub(crate) fn hfn() -> Option<HammerFns> {
    *lock(&HFN)
}

} // mod bind

// ---------------------------------------------------------------------------------------------
// The production reader (Windows tools build only)
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
pub(crate) mod win {
    use core::ffi::{c_char, c_int, c_void};
    use core::mem::offset_of;
    use core::ptr;

    use s2sdk_sys::generated::{
        CBaseEditorApp, CDmAttribute, CDmElement, CHammerEditorSession, CMapDoc, CMapDocHistory,
        CMapDocSession, CMapEntityIONode, CMapNode, CUtlSymbolLarge, CUtlVector_CEntityHandle,
        DmElementHandle_t, IDataModel, MapEntityConnection_t, UndoGroup_t,
    };

    use crate::ops::ident::obj_is;

    use super::bind::{hammer_bind, hfn};

    use super::{
        active_doc_with, cstr, doc_editable_with, flag_byte, history_top_info, lock, mem_ok,
        name_is, pins, probe_str, Chain, ChildEntry, ChildList, Conn, ConnList, CStr, Doc,
        DocFacts, EntityRead, FpSample, Hammer, HistoryInfo, Node, Nodes, UndoGroupView,
        CHILDREN_LIST_MAX, CONN_VEC_SANE, ENTITY_STR_CAP, RM_INDEX_MAX,
    };
    use crate::{log, modules};

    /// The C's `HG_HAMMERAPP_OBJ` / `HG_*` globals and the `HV_*` vtables live in `pins`; this
    /// module only spells the C's own reason strings and the module's identity path.
    static APP: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

    /// The C asserts `sizeof(S2ConnectionSrc) == 80` (`s2sdk.h:447`); the generated
    /// element the live vector holds must be the same record, and the reader strides by it.
    const _: () = assert!(core::mem::size_of::<MapEntityConnection_t>() == 80);

    /// The production reader: one stateless object; its state is the module's pins and the
    /// process-wide bind/fingerprint statics.
    pub(crate) struct LiveHammer;

    pub(super) static LIVE: LiveHammer = LiveHammer;

    fn base() -> usize {
        modules::live().base(pins::PIN_HAMMER) as usize
    }

    /// `hb + pin.value`: one pinned RVA resolved against the verified load base.
    fn at(pin: pins::Pin) -> usize {
        base() + pin.value as usize
    }

    /// `resolve_hammer` (`mcp_bridge.c:1693-1711`): the `CHammerApp *` through hammer.dll's
    /// exported `CreateInterface("ToolSystem2_001")`, every identity check kept.
    fn resolve_hammer() -> Option<usize> {
        if let Some(app) = *lock(&APP) {
            return Some(app);
        }
        if !modules::live().is_verified(pins::PIN_HAMMER) {
            log::line("doc: hammer not verified"); // :1695
            return None;
        }
        let hb = modules::live().base(pins::PIN_HAMMER);
        let name = pins::PIN_MODULES[usize::from(pins::PIN_HAMMER)].name;
        // The image must be the verified one. The one game-module finder opens the game's own
        // `hammer.dll` (never Metamod's loader stub, never a guess between two images), and its
        // base must be the base the gate verified - the C's
        // `GetModuleHandleA(name) != g_modBase[PIN_HAMMER]` (`:1697-1698`), asked of the finder
        // instead of the ambiguous by-name query.
        let Some(module) = s2sdk::module::Module::find_game(name).ok().flatten() else {
            log::line("doc: hammer module handle changed"); // :1698
            return None;
        };
        if module.image().base() != hb {
            log::line("doc: hammer module handle changed"); // :1698
            return None;
        }
        let Some(sym) = module.symbol("CreateInterface") else {
            log::line("doc: hammer CreateInterface export missing"); // :1700
            return None;
        };
        // `S2CreateInterfaceFn` (`s2sdk.h:60`).
        type CreateInterfaceFn =
            unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
        // SAFETY: `sym` is the module's exported factory, found by name in the verified module.
        let factory: CreateInterfaceFn = unsafe { core::mem::transmute(sym) };
        let mut ret: c_int = -1;
        // SAFETY: the factory is a plain C call; the version string outlives the call and
        // `ret` is a valid out-parameter.
        let app = unsafe { factory(c"ToolSystem2_001".as_ptr(), &mut ret) };
        if app.is_null() || ret != 0 {
            log::line(&format!("doc: ToolSystem2_001 unresolved, ret={ret}")); // :1703
            return None;
        }
        if app as usize != at(pins::HG_HAMMERAPP_OBJ) {
            log::line("doc: hammer app identity mismatch"); // :1704
            return None;
        }
        if !obj_is(app, pins::PIN_HAMMER, pins::HV_HAMMERAPP_VT.value as u32) {
            log::line("doc: hammer app vtable mismatch"); // :1705
            return None;
        }
        hammer_bind(); // :1706
        *lock(&APP) = Some(app as usize);
        log::line("doc: hammer app resolved (ToolSystem2_001 export)"); // :1708
        Some(app as usize)
    }

    /// `data_model` (`mcp_bridge.c:1938-1941`).
    fn data_model() -> Option<*mut IDataModel> {
        // SAFETY: `HG_DATAMODEL` is a pinned global in the verified module; the read is the
        // C's `*(IDataModel **)(base + HG_DATAMODEL)`.
        let dm = unsafe { *((at(pins::HG_DATAMODEL)) as *const *mut IDataModel) };
        if obj_is(dm.cast(), pins::PIN_HAMMER, pins::HV_DATAMODEL_VT.value as u32) {
            Some(dm)
        } else {
            None
        }
    }

    /// `dm_intern` (`:1942-1947`): `IDataModel::GetSymbol`, zero symbol without a model.
    fn dm_intern(name: &core::ffi::CStr) -> CUtlSymbolLarge {
        let mut sym = CUtlSymbolLarge {
            m_pString: ptr::null(),
        };
        let Some(dm) = data_model() else {
            return sym;
        };
        // SAFETY: `dm` passed `obj_is` for the pinned IDataModel vtable, so the vtable and its
        // GetSymbol slot are the game's own; `sym` is a valid out-parameter.
        unsafe {
            if let Some(get_symbol) = IDataModel::vtable(dm).GetSymbol {
                get_symbol(dm, &mut sym, name.as_ptr());
            }
        }
        sym
    }

    /// `dm_element` (`:3272-3275`).
    fn dm_element(handle: u32) -> Option<*mut CDmElement> {
        let dm = data_model()?;
        // SAFETY: `dm` passed `obj_is`; GetElement is a pinned slot of that vtable.
        let element = unsafe {
            IDataModel::vtable(dm).GetElement.map(|get| get(dm, DmElementHandle_t(handle)))?
        };
        if element.is_null() {
            None
        } else {
            Some(element)
        }
    }

    /// `is_map_node` (`:3276-3281`).
    fn is_map_node(element: *mut CDmElement) -> bool {
        if element.is_null() || !mem_ok(element.cast()) {
            return false;
        }
        // SAFETY: `HG_MAPNODE_TYPE_SYM` holds the address of the type symbol's C string; the
        // C reads the same slot (`:3279`).
        let symbol = CUtlSymbolLarge {
            m_pString: unsafe { *((at(pins::HG_MAPNODE_TYPE_SYM)) as *const *const c_char) },
        };
        // SAFETY: `mem_ok` just proved the element's first byte readable, and every element
        // whose vptr is read here is a DmElement with a live vtable.
        unsafe {
            let vtable = CDmElement::vtable(element);
            vtable.IsA.is_some_and(|is_a| is_a(element, &symbol))
        }
    }

    /// `elem_attr` (`:3282-3292`): the element's attribute with exactly this name.
    fn elem_attr(element: *mut CDmElement, name: &core::ffi::CStr) -> Option<*mut CDmAttribute> {
        // SAFETY: an element the caller identity-checked; the attribute chain is walked with
        // the C's own mem_ok guards below.
        let mut attr = unsafe { (*element).m_pAttributes };
        for _guard in 0..4096 {
            if attr.is_null() {
                return None;
            }
            let end = (attr as *const u8).wrapping_add(core::mem::size_of::<CDmAttribute>() - 1);
            if !mem_ok(attr.cast()) || !mem_ok(end.cast()) {
                return None;
            }
            // SAFETY: both ends of the attribute were just proven readable.
            let attr_name = unsafe { (*attr).m_Name.m_pString };
            if !attr_name.is_null() && mem_ok(attr_name.cast()) {
                // SAFETY: the first byte was proven readable; `name_is` re-probes every byte it
                // walks and stops at the first difference or the shared NUL, as `seq` does.
                if unsafe { name_is(attr_name.cast(), name) } {
                    return Some(attr);
                }
            }
            // SAFETY: the attribute was proven readable.
            attr = unsafe { (*attr).m_pNext };
        }
        None
    }

    /// `attr_cstr` (`:1967-1977`): the `char *` a String attribute holds.
    ///
    /// `Some` is the C's non-null answer; an empty `CUtlSymbolLarge` is a null pointer and maps
    /// to the empty string, as the C's `return ""` does.
    fn attr_cstr(attr: *mut CDmAttribute) -> Option<*const c_char> {
        static EMPTY: &[u8] = b"\0";
        if attr.is_null() {
            return None;
        }
        // SAFETY: the caller passed an attribute from `elem_attr`/`FindAttribute`.
        let storage = unsafe { (*attr).m_pData } as *const u8;
        if storage.is_null() {
            return None;
        }
        if !mem_ok(storage.cast()) || !mem_ok(storage.wrapping_add(7).cast()) {
            return None;
        }
        // SAFETY: the 8-byte symbol was proven readable.
        let symbol = unsafe { *(storage as *const *const c_char) };
        if symbol.is_null() {
            return Some(EMPTY.as_ptr().cast());
        }
        mem_ok(symbol.cast()).then_some(symbol)
    }

    /// `probe_str` into a fresh buffer, as the bytes the C would hand to `sb_addjson`
    /// (empty when unreadable).
    ///
    /// The raw bytes, not lossy text: the C's `sb_addjson` does the invalid-UTF-8 replacement,
    /// so doing the conversion here would replace twice (or differently) from the C.
    fn probe_text(source: *const c_char, cap: usize) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        // SAFETY: probe_str re-probes every page boundary; a game string is exactly what it is
        // for.
        let got = unsafe { probe_str(source.cast(), &mut buf) };
        if got == 0 {
            Vec::new()
        } else {
            cstr(&buf).to_vec()
        }
    }

    /// `child_string` (`:3477-3484`): the child's `name` attribute as text.
    pub(crate) fn child_string(child: *mut CDmElement, name: &core::ffi::CStr, cap: usize) -> Vec<u8> {
        let sym = dm_intern(name);
        if sym.m_pString.is_null() {
            return Vec::new();
        }
        let Some(table) = hfn() else {
            return Vec::new();
        };
        // SAFETY: `child` is a readable element and `table.find_attribute` is the verified
        // HFN_FIND_ATTRIBUTE function; `sym` is the interned symbol.
        let attr = unsafe { (table.find_attribute)(child.cast(), &sym) } as *mut CDmAttribute;
        match attr_cstr(attr) {
            Some(text) => probe_text(text, cap),
            None => Vec::new(),
        }
    }

    /// `node_id_of` (`:3281-3287`).
    fn node_id(node: *mut CMapNode) -> Option<i32> {
        let attr = elem_attr(node.cast(), c"nodeID")?;
        // SAFETY: `attr` was proven readable by `elem_attr`.
        if unsafe { (*attr).m_nFlags } & ATTR_TYPE_MASK != AT_AT_INT {
            return None;
        }
        // SAFETY: `attr` is readable; `m_pData` is the value storage the C reads as `int *`.
        let value = unsafe { (*attr).m_pData } as *const i32;
        if value.is_null() || !mem_ok(value.cast()) || !mem_ok((value as *const u8).wrapping_add(3).cast()) {
            return None;
        }
        // SAFETY: the four bytes were just proven readable.
        Some(unsafe { value.read_unaligned() })
    }

    /// `node_origin` / `node_angles` / `node_scales` (`:3288-3312`), over one generated field.
    fn node_vec(node: *mut CMapNode, which: usize) -> Option<[f32; 3]> {
        // SAFETY: the caller passed a node a walk reached; `addr_of!` computes the field
        // addresses without creating a reference to game memory. `Vector` and `QAngle` are both
        // `repr(C)` triples of f32 (`generated/tier0.rs`), so the read is the C's
        // `m_Origin.x/.y/.z` access. `z` is the field's own address, not `start + 8`.
        let (start, z) = unsafe {
            match which {
                0 => (
                    ptr::addr_of!((*node).base.m_Origin).cast::<f32>(),
                    ptr::addr_of!((*node).base.m_Origin.z),
                ),
                1 => (
                    ptr::addr_of!((*node).base.m_Angles).cast::<f32>(),
                    ptr::addr_of!((*node).base.m_Angles.z),
                ),
                _ => (
                    ptr::addr_of!((*node).base.m_Scales).cast::<f32>(),
                    ptr::addr_of!((*node).base.m_Scales.z),
                ),
            }
        };
        if !mem_ok(start.cast()) || !mem_ok(z.cast()) {
            return None;
        }
        // SAFETY: the three f32 components were just proven readable.
        Some(unsafe { start.cast::<[f32; 3]>().read_unaligned() })
    }

    /// `node_class` (`:3297-3306`).
    fn node_class(node: *mut CMapNode) -> &'static str {
        let object = node.cast();
        if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPMESH_VT.value as u32) {
            "mesh"
        } else if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPENTITY_VT.value as u32) {
            "entity"
        } else if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPGROUP_VT.value as u32) {
            "group"
        } else if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPPREFAB_VT.value as u32) {
            "prefab"
        } else if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPINSTANCE_VT.value as u32) {
            "instance"
        } else if obj_is(object, pins::PIN_HAMMER, pins::HV_MAPWORLD_VT.value as u32) {
            "world"
        } else {
            "other"
        }
    }

    /// `CHammerPolyMesh`'s two component counts (`:3207-3208`): the mesh object plus the
    /// pinned `HOFF_MESH_POLYMESH` / `HOFF_POLYMESH_VERTS` / `HOFF_POLYMESH_FACES` offsets
    /// (`s2sdk.h:482-489`), read with the guards the C omits.
    fn mesh_slots(node: *mut CMapNode) -> Option<(i32, i32)> {
        let poly = (node as *const u8).wrapping_add(pins::HOFF_MESH_POLYMESH.value as usize);
        let verts = poly.wrapping_add(pins::HOFF_POLYMESH_VERTS.value as usize);
        let faces = poly.wrapping_add(pins::HOFF_POLYMESH_FACES.value as usize);
        if !mem_ok(verts.cast()) || !mem_ok(verts.wrapping_add(3).cast()) {
            return None;
        }
        if !mem_ok(faces.cast()) || !mem_ok(faces.wrapping_add(3).cast()) {
            return None;
        }
        // SAFETY: both 4-byte counts were just proven readable; `S2ComponentList` is `{int
        // count}` (`s2sdk.h:479`).
        unsafe {
            Some((
                (verts as *const i32).read_unaligned(),
                (faces as *const i32).read_unaligned(),
            ))
        }
    }

    /// `CMapNode`'s child handles (`CMapNode_ChildHandles`, `s2sdk.h:497`): the
    /// generated `CUtlVector_CEntityHandle` shape over `m_Children`, whose IR type is bytes.
    ///
    /// Returns the vector header's address after proving its count readable; the caller reads
    /// every field through the pointer, never through a `&` into game memory.
    fn child_vector(node: *mut CMapNode) -> Option<*const CUtlVector_CEntityHandle> {
        // SAFETY: `addr_of!` computes the field address only (no reference into game memory);
        // the cast is the C's `(u32 *)n->m_Children.m_pMemory` view of the same `CUtlVector`
        // layout, asserted size-for-size by the C (`S2HandleVec`).
        let vector = unsafe {
            ptr::addr_of!((*node).m_Children).cast::<CUtlVector_CEntityHandle>()
        };
        // SAFETY: `addr_of!` again - the count member's address, guarded like `node_child_count`
        // (`:3360-3364`).
        let size = unsafe { ptr::addr_of!((*vector).m_Size) };
        if !mem_ok(size.cast()) || !mem_ok((size as *const u8).wrapping_add(3).cast()) {
            return None;
        }
        Some(vector)
    }

    /// The children of one node as map nodes (`geo_walk`'s guard block, `:3160-3166`).
    fn children(node: *mut CMapNode) -> Option<Vec<Node>> {
        let vector = child_vector(node)?;
        // SAFETY: `child_vector` proved the 4-byte count readable.
        let count = unsafe { ptr::addr_of!((*vector).m_Size).read_unaligned() };
        if count <= 0 {
            return Some(Vec::new());
        }
        if count > 1_000_000 {
            return None;
        }
        // SAFETY: `addr_of!` gives the member's address; the whole 8-byte pointer is proven
        // readable before it is read (`:3369` checks it, `conn_vec` checks the same member).
        let memory = unsafe { ptr::addr_of!((*vector).m_pMemory) };
        if !mem_ok(memory.cast())
            || !mem_ok(
                (memory as *const u8)
                    .wrapping_add(core::mem::size_of::<*mut c_void>() - 1)
                    .cast(),
            )
        {
            return None;
        }
        // SAFETY: the pointer member was just proven readable.
        let handles = unsafe { memory.read_unaligned() } as *const u32;
        if handles.is_null() || !mem_ok(handles.cast()) {
            return None;
        }
        // The element is the C's `u32` handle (`CMapNode_ChildHandles`), so the stride is its
        // size, never a hand-written 4.
        let last = (handles as *const u8)
            .wrapping_add((count as usize - 1) * core::mem::size_of::<u32>());
        if !mem_ok(last.cast()) {
            return None;
        }
        let mut out = Vec::new();
        for index in 0..count as usize {
            // SAFETY: the whole handle array was proven readable.
            let handle = unsafe { handles.add(index).read_unaligned() };
            let Some(element) = dm_element(handle) else {
                continue;
            };
            if !mem_ok(element.cast()) || !is_map_node(element) {
                continue;
            }
            out.push(Node(element as usize));
        }
        Some(out)
    }

    /// `geo_world` (`:3212-3215`).
    fn world(doc: Doc) -> Option<Node> {
        let doc = doc.0 as *mut CMapDoc;
        // SAFETY: `doc` passed the `active_doc` identity chain.
        let world = unsafe { (*doc).m_pWorld };
        if world.is_null() || !mem_ok(world.cast()) {
            None
        } else {
            Some(Node(world as usize))
        }
    }

    /// `history_top_info` over one live history (`:1810-1824`).
    fn history_info(history: usize) -> HistoryInfo {
        let history = history as *mut CMapDocHistory;
        if history.is_null() {
            return HistoryInfo::default();
        }
        // SAFETY: the caller's history came from `GetUndoHistory`/the active-history global and
        // passed `obj_is` where the C checks it. `m_unk001` is `CUndoHistory`'s
        // `CUtlVector<UndoGroup_t *>` (`CMapDocHistory_Count`/`_Groups`, `s2sdk.h:407`);
        // `addr_of!` computes its address and no reference into game memory is created here.
        let vector = unsafe {
            ptr::addr_of!((*history).base.m_unk001).cast::<CUtlVector_CEntityHandle>()
        };
        // SAFETY: the count member's address; only its 4 bytes are probed before the read.
        let size = unsafe { ptr::addr_of!((*vector).m_Size) };
        if !mem_ok(size.cast()) {
            return HistoryInfo::default();
        }
        // SAFETY: the 4-byte count was just proven readable.
        let count = unsafe { size.read_unaligned() };
        if count <= 0 {
            return history_top_info(0, None, 128);
        }
        // SAFETY: the pointer member's address; the whole 8-byte pointer is probed first.
        let memory = unsafe { ptr::addr_of!((*vector).m_pMemory) };
        if !mem_ok(memory.cast())
            || !mem_ok(
                (memory as *const u8)
                    .wrapping_add(core::mem::size_of::<*mut c_void>() - 1)
                    .cast(),
            )
        {
            return history_top_info(count as u32, None, 128);
        }
        // SAFETY: the pointer member was just proven readable.
        let groups = unsafe { memory.read_unaligned() } as *const *mut UndoGroup_t;
        if groups.is_null() || !mem_ok(groups.cast()) {
            return history_top_info(count as u32, None, 128);
        }
        // The C reads `CMapDocHistory_Groups(h)[n - 1]` with no guard at all (`:1817`): only
        // `groups[0]`'s address is proven when the base is probed. This port also probes the
        // one slot it reads; that cannot change a reply for a well-formed array, and a corrupt
        // count answers "no top group" instead of faulting.
        let slot = (groups as *const u8)
            .wrapping_add((count as usize - 1) * core::mem::size_of::<*mut UndoGroup_t>());
        if !mem_ok(slot.cast())
            || !mem_ok(
                slot.wrapping_add(core::mem::size_of::<*mut UndoGroup_t>() - 1)
                    .cast(),
            )
        {
            return history_top_info(count as u32, None, 128);
        }
        // SAFETY: the slot was just proven readable.
        let entry = unsafe { slot.cast::<*mut UndoGroup_t>().read_unaligned() };
        if entry.is_null() || !mem_ok(entry.cast()) {
            return history_top_info(count as u32, None, 128);
        }
        // SAFETY: the entry's first byte was proven readable. The C reads `m_nSerial` and
        // `m_szName` unguarded (`:1818-1821`); this copies them into locals in one go, so no
        // reference into the game object outlives the read.
        let serial = unsafe { ptr::addr_of!((*entry).m_nSerial).read_unaligned() };
        let name_ptr = unsafe { ptr::addr_of!((*entry).m_szName).cast::<u8>() };
        let mut name = [0u8; 128];
        if mem_ok(name_ptr.cast())
            && mem_ok(name_ptr.wrapping_add(name.len() - 1).cast())
        {
            // SAFETY: all 128 label bytes were just proven readable.
            unsafe { ptr::copy_nonoverlapping(name_ptr, name.as_mut_ptr(), name.len()) };
        }
        history_top_info(count as u32, Some(UndoGroupView { serial, name: &name }), 128)
    }

    /// `conn_vec` (`:3555-3569`) + `conn_list_json` (`:3612-3632`).
    /// `conn_vec` (`mcp_bridge.c:3555-3569`): the live connection vector's count and data.
    ///
    /// Both bounds are the C's: the count must be in `0..=CONN_VEC_SANE`, the pointer member is
    /// probed as a whole, and a non-empty vector with a null data pointer is refused.
    fn conn_vec(node: *mut CMapNode) -> Option<(i32, *const u8)> {
        // SAFETY: `CMapEntityIONode::m_Connections` is the generated `CUtlVector_CEntityHandle`
        // shape, and `addr_of!` computes its address only (no reference into game memory). The
        // C casts the same node pointer to `S2_CMapEntityIONode` and reads the same members.
        let vector = unsafe {
            ptr::addr_of!((*node.cast::<CMapEntityIONode>()).m_Connections)
                .cast::<CUtlVector_CEntityHandle>()
        };
        // SAFETY: the count member's address; its 4 bytes are probed before the read.
        let size = unsafe { ptr::addr_of!((*vector).m_Size) };
        if !mem_ok(size.cast()) || !mem_ok((size as *const u8).wrapping_add(3).cast()) {
            return None;
        }
        // SAFETY: the 4-byte count was just proven readable.
        let count = unsafe { size.read_unaligned() };
        if !(0..=CONN_VEC_SANE).contains(&count) {
            return None;
        }
        // SAFETY: the pointer member's address, guarded as the C's `conn_vec` does (`:3560`).
        let memory = unsafe { ptr::addr_of!((*vector).m_pMemory) };
        if !mem_ok(memory.cast())
            || !mem_ok(
                (memory as *const u8)
                    .wrapping_add(core::mem::size_of::<*mut c_void>() - 1)
                    .cast(),
            )
        {
            return None;
        }
        // SAFETY: the pointer member was just proven readable.
        let data = unsafe { memory.read_unaligned() } as *const u8;
        if count > 0 && data.is_null() {
            return None;
        }
        Some((count, data))
    }

    /// `conn_at` + `conn_entry_ok` + `sb_add_conn_fields`' reads (`mcp_bridge.c:3571-3604`): one
    /// vector entry, copied out by value.
    ///
    /// # Safety
    ///
    /// `data` came from [`conn_vec`] and `index` is a live index the caller bounds-checked; every
    /// byte of the entry is re-probed before the copy.
    unsafe fn read_conn(data: *const u8, index: usize) -> Option<Conn> {
        let entry = data.wrapping_add(core::mem::size_of::<MapEntityConnection_t>() * index);
        let last = entry.wrapping_add(core::mem::size_of::<MapEntityConnection_t>() - 1);
        if !mem_ok(entry.cast()) || !mem_ok(last.cast()) {
            return None;
        }
        // SAFETY: the whole 80-byte entry was just proven readable, and `read_unaligned` copies
        // it out by value - no reference into the game vector. The generated layout is the C's
        // `S2ConnectionSrc` (both 80 bytes, same field order).
        let connection = unsafe { entry.cast::<MapEntityConnection_t>().read_unaligned() };
        let mut cut = false;
        let mut text = |source: *const c_char| -> Vec<u8> {
            let mut buf = [0u8; 256];
            // SAFETY: `probe_str` re-probes every page boundary.
            let got = unsafe { probe_str(source.cast(), &mut buf) };
            if got == 0 {
                return Vec::new();
            }
            if got < 0 {
                cut = true;
            }
            cstr(&buf).to_vec()
        };
        let output = text(connection.m_unk002.m_pString);
        let target = text(connection.m_unk003.m_pString);
        let input = text(connection.m_unk004.m_pString);
        let param = text(connection.m_unk005.m_pString);
        Some(Conn {
            output,
            target_type: connection.m_unk006,
            target,
            input,
            param,
            delay: f32::from_bits(connection.m_unk008),
            times_to_fire: connection.m_unk009,
            cut,
        })
    }

    /// `conn_list_json` (`mcp_bridge.c:3612-3632`).
    fn conn_list(node: *mut CMapNode) -> ConnList {
        if !obj_is(node.cast(), pins::PIN_HAMMER, pins::HV_MAPENTITY_VT.value as u32) {
            return ConnList::Null;
        }
        let Some((count, data)) = conn_vec(node) else {
            return ConnList::Null;
        };
        let mut entries = Vec::new();
        let taken = (count as usize).min(RM_INDEX_MAX);
        for index in 0..taken {
            // SAFETY: `conn_vec` gave the live vector and `index` is inside its count (the walk
            // stops at the first unreadable entry, exactly as the C's `conn_entry_ok` does).
            let Some(connection) = (unsafe { read_conn(data, index) }) else {
                break;
            };
            entries.push(connection);
        }
        let total = if entries.len() == RM_INDEX_MAX && count as usize > entries.len() {
            Some(count)
        } else {
            None
        };
        ConnList::List { entries, total }
    }

    /// `entity_children_json` (`:3640-3691`).
    fn child_list(node: *mut CMapNode) -> ChildList {
        let Some(vector) = child_vector(node) else {
            return ChildList::Null;
        };
        // SAFETY: `child_vector` proved the 4-byte count readable.
        let count = unsafe { ptr::addr_of!((*vector).m_Size).read_unaligned() };
        if !(0..=1_000_000).contains(&count) {
            return ChildList::Null;
        }
        let mut handles: *const u32 = ptr::null();
        if count > 0 {
            // SAFETY: the pointer member's address; the whole 8-byte pointer is probed.
            let memory = unsafe { ptr::addr_of!((*vector).m_pMemory) };
            if !mem_ok(memory.cast())
                || !mem_ok(
                    (memory as *const u8)
                        .wrapping_add(core::mem::size_of::<*mut c_void>() - 1)
                        .cast(),
                )
            {
                return ChildList::Null;
            }
            // SAFETY: the pointer member was just proven readable.
            handles = unsafe { memory.read_unaligned() } as *const u32;
            // The element is the C's `u32` handle, so the stride is its size, never a 4.
            let last = (handles as *const u8)
                .wrapping_add((count as usize - 1) * core::mem::size_of::<u32>());
            if handles.is_null() || !mem_ok(handles.cast()) || !mem_ok(last.cast()) {
                return ChildList::Null;
            }
        }
        let mut entries = Vec::new();
        let mut total = 0i32;
        for index in 0..count as usize {
            // SAFETY: the handle array was proven readable above.
            let handle = unsafe { handles.add(index).read_unaligned() };
            let Some(element) = dm_element(handle) else {
                continue;
            };
            if !mem_ok(element.cast()) || !is_map_node(element) {
                continue;
            }
            total += 1;
            if entries.len() >= CHILDREN_LIST_MAX {
                continue;
            }
            entries.push(ChildEntry {
                id: node_id(element as *mut CMapNode),
                class: node_class(element as *mut CMapNode),
            });
        }
        let cut = if entries.len() == CHILDREN_LIST_MAX && total > entries.len() as i32 {
            Some(total)
        } else {
            None
        };
        ChildList::List { entries, total: cut }
    }

    /// `entity_value_json` (`:3455-3475`) for one attribute.
    fn attr_value(attr: *mut CDmAttribute) -> super::AttrValue {
        // SAFETY: the caller took the attribute from the element's readable chain.
        let kind = unsafe { (*attr).m_nFlags } & ATTR_TYPE_MASK;
        let supported = matches!(kind, AT_AT_INT | AT_AT_FLOAT | AT_AT_BOOL | AT_AT_STRING);
        if kind == AT_AT_STRING {
            // SAFETY: `attr_cstr` proves every pointer it follows.
            match attr_cstr(attr) {
                Some(text) => {
                    let mut buf = [0u8; ENTITY_STR_CAP];
                    // SAFETY: `probe_str` re-probes every page boundary.
                    let got = unsafe { probe_str(text.cast(), &mut buf) };
                    if got == 1 {
                        // Raw bytes: `sb_addjson` is what performs the C's UTF-8 repair
                        // (`:3455`, `sb_addjson`), so converting to text here would differ.
                        super::AttrValue::Str(cstr(&buf).to_vec())
                    } else {
                        super::AttrValue::Null {
                            unsupported: !supported,
                            cut: got < 0,
                        }
                    }
                }
                None => super::AttrValue::Null {
                    unsupported: !supported,
                    cut: false,
                },
            }
        } else if matches!(kind, AT_AT_INT | AT_AT_FLOAT | AT_AT_BOOL) {
            // SAFETY: `attr` is readable; `m_pData` points at the typed storage.
            let storage = unsafe { (*attr).m_pData } as *const u8;
            let readable = !storage.is_null()
                && mem_ok(storage.cast())
                && mem_ok(storage.wrapping_add(3).cast());
            if !readable {
                return super::AttrValue::Null {
                    unsupported: !supported,
                    cut: false,
                };
            }
            // SAFETY: the four bytes were just proven readable.
            unsafe {
                if kind == AT_AT_INT {
                    super::AttrValue::Int((storage as *const i32).read_unaligned())
                } else if kind == AT_AT_FLOAT {
                    super::AttrValue::Float((storage as *const f32).read_unaligned())
                } else {
                    super::AttrValue::Bool((storage as *const i32).read_unaligned() != 0)
                }
            }
        } else {
            super::AttrValue::Null {
                unsupported: !supported,
                cut: false,
            }
        }
    }

    /// The `keyValues` walk (`:3760-3798`): the element's attribute chain, `id`/`name` dropped.
    fn entity_attrs(element: *mut CDmElement) -> (Vec<(Vec<u8>, super::AttrValue)>, bool) {
        let mut attrs = Vec::new();
        let mut truncated = false;
        // SAFETY: the element is the caller's identity-checked properties element; every
        // attribute step is guarded below, like `elem_attr`.
        let mut attr = unsafe { (*element).m_pAttributes };
        for _guard in 0..4096 {
            if attr.is_null() {
                break;
            }
            let end = (attr as *const u8).wrapping_add(core::mem::size_of::<CDmAttribute>() - 1);
            if !mem_ok(attr.cast()) || !mem_ok(end.cast()) {
                break;
            }
            // SAFETY: the attribute was proven readable.
            let name_ptr = unsafe { (*attr).m_Name.m_pString };
            let mut name_buf = [0u8; 512];
            let named = !name_ptr.is_null()
                && mem_ok(name_ptr.cast())
                // SAFETY: `probe_str` re-probes every page boundary.
                && unsafe { probe_str(name_ptr.cast(), &mut name_buf) } != 0;
            if named {
                let name = cstr(&name_buf).to_vec();
                if !seq(&name, b"id") && !seq(&name, b"name") {
                    if attrs.len() >= super::ENTITY_MAX_ATTRS {
                        truncated = true;
                        break;
                    }
                    // The name's bytes, exactly as `sb_addjson(out, nm)` receives them.
                    attrs.push((name, attr_value(attr)));
                }
            }
            // SAFETY: the attribute was proven readable.
            attr = unsafe { (*attr).m_pNext };
        }
        (attrs, truncated)
    }

    use super::seq;

    /// `GetEntityProperties` with the C's readable check (`:3701-3703`).
    pub(crate) fn entity_properties(node: *mut CMapNode) -> Option<*mut CDmElement> {
        let table = hfn()?;
        // SAFETY: `table.get_entity_properties` is the verified HFN function; `node` is a
        // node the walk reached.
        let element = unsafe { (table.get_entity_properties)(node.cast()) } as *mut CDmElement;
        if element.is_null() || !mem_ok(element.cast()) {
            None
        } else {
            Some(element)
        }
    }

    impl Chain for LiveHammer {
        fn module_reason(&self) -> Option<&'static str> {
            if modules::live().is_verified(pins::PIN_HAMMER) {
                return None;
            }
            Some(match modules::live().state(pins::PIN_HAMMER) {
                Some(modules::State::Mismatch) => "hammer_mismatch",
                Some(modules::State::Verifying) => "hammer_verifying",
                _ => "hammer_not_loaded",
            })
        }

        fn app(&self) -> Option<usize> {
            resolve_hammer()
        }

        fn session(&self, app: usize) -> Option<usize> {
            // SAFETY: `app` is the pinned static `CHammerApp`, whose layout is
            // `S2_CBaseEditorApp` with the generated `m_pActiveSession` member
            // (`s2sdk.h:335`, `S2_OBJECT(CHammerApp, S2_CBaseEditorApp, ..)`).
            let session =
                unsafe { (*(app as *const CBaseEditorApp)).m_pActiveSession } as usize;
            (session != 0).then_some(session)
        }

        fn doc_session(&self, session: usize) -> Option<usize> {
            // SAFETY: the session passed `obj_is(HV_SESSION_VT)`; `m_pDocSession` is the
            // generated CHammerEditorSession member the C reads (`mcp_bridge.c:1725`).
            let doc_session =
                unsafe { (*(session as *const CHammerEditorSession)).m_pDocSession } as usize;
            (doc_session != 0).then_some(doc_session)
        }

        fn owner(&self, doc_session: usize) -> usize {
            // SAFETY: the doc session passed `obj_is(HV_DOCSESSION_VT)`.
            unsafe { (*(doc_session as *const CMapDocSession)).m_pOwner as usize }
        }

        fn doc_of(&self, doc_session: usize) -> usize {
            // SAFETY: the doc session passed `obj_is`; the document is embedded at `m_Doc`
            // (`CMapDocSession_Doc`, `s2sdk.h:388`).
            unsafe {
                ptr::addr_of!((*(doc_session as *const CMapDocSession)).m_Doc) as usize
            }
        }

        fn doc_backptr(&self, doc: usize) -> usize {
            // SAFETY: the embedded document address; `m_pDocSession` is the generated member.
            unsafe { (*(doc as *const CMapDoc)).m_pDocSession as usize }
        }

        fn is_of(&self, object: usize, vtable: u32) -> bool {
            obj_is(object as *mut c_void, pins::PIN_HAMMER, vtable)
        }

        fn root(&self, doc: usize) -> usize {
            // SAFETY: `doc` passed `obj_is(HV_DOC_VT)`.
            unsafe { (*(doc as *const CMapDoc)).m_pRoot as usize }
        }

        fn active_flag(&self, doc: usize) -> bool {
            // SAFETY: `doc` passed `obj_is(HV_DOC_VT)`. The C reads `doc->m_bActive` as a `u32`
            // (`:6246`); this reads the one field byte and only its truthiness crosses, so a
            // garbage byte can never form an invalid Rust `bool`.
            unsafe {
                flag_byte(ptr::addr_of!((*(doc as *const CMapDoc)).m_bActive).cast::<u8>())
            }
        }

        fn selection(&self, doc: usize) -> usize {
            // SAFETY: `doc` passed `obj_is(HV_DOC_VT)`.
            unsafe { (*(doc as *const CMapDoc)).m_pSelection as usize }
        }

        fn scope_depth(&self, doc: usize) -> u32 {
            // SAFETY: `doc` passed `obj_is(HV_DOC_VT)`; `m_Scopes` is the generated
            // `CUtlVector_CEntityHandle` shape (`s2sdk.h:384`).
            let vector = unsafe {
                &*(ptr::addr_of!((*(doc as *const CMapDoc)).m_EditScopeStack.m_Scopes)
                    .cast::<CUtlVector_CEntityHandle>())
            };
            vector.m_Size as u32
        }

        fn undo_history(&self, doc: usize) -> usize {
            let Some(table) = hfn() else { return 0 };
            // SAFETY: the verified HFN_GET_UNDO_HISTORY function, called with the document the
            // C passes (`mcp_bridge.c:1791`).
            unsafe { (table.get_undo_history)(doc as *mut c_void) as usize }
        }

        fn active_history(&self) -> usize {
            // SAFETY: `HG_ACTIVE_HISTORY` is the pinned global holding the active history
            // pointer (`mcp_bridge.c:1798`).
            unsafe { *((base() + pins::HG_ACTIVE_HISTORY.value as usize) as *const usize) }
        }
    }

    impl Nodes for LiveHammer {
        fn children(&self, node: Node) -> Option<Vec<Node>> {
            children(node.0 as *mut CMapNode)
        }

        fn class(&self, node: Node) -> &'static str {
            node_class(node.0 as *mut CMapNode)
        }

        fn id(&self, node: Node) -> Option<i32> {
            node_id(node.0 as *mut CMapNode)
        }

        fn origin(&self, node: Node) -> Option<[f32; 3]> {
            node_vec(node.0 as *mut CMapNode, 0)
        }

        fn mesh_slots(&self, node: Node) -> Option<(i32, i32)> {
            mesh_slots(node.0 as *mut CMapNode)
        }

        fn world(&self, doc: Doc) -> Option<Node> {
            world(doc)
        }
    }

    impl Hammer for LiveHammer {
        fn active_doc(&self) -> Result<Doc, &'static str> {
            active_doc_with(self)
        }

        fn fp_sample(&self) -> FpSample {
            let mut sample = FpSample::default();
            if !modules::live().is_verified(pins::PIN_HAMMER) {
                return sample;
            }
            // The C samples only once `g_hammer` is set (`:1747`): the tick must not resolve
            // the app itself, or the first doc.active would report an epoch the C has not
            // counted yet.
            let Some(app) = *lock(&APP) else {
                return sample;
            };
            let Some(session) = <Self as Chain>::session(self, app) else {
                return sample;
            };
            let Some(doc_session) = <Self as Chain>::doc_session(self, session) else {
                return sample;
            };
            sample.doc_session = doc_session;
            let doc = <Self as Chain>::doc_of(self, doc_session) as *mut CMapDoc;
            // SAFETY: the chain above is exactly `doc_fingerprint`'s (`:1748-1762`), over the
            // objects it reads unguarded.
            unsafe {
                let root = (*doc).m_pRoot;
                sample.root = root as usize;
                sample.path_ptr = (*doc).m_Path.m_pString as usize;
                if !root.is_null() {
                    sample.root_handle = (*root).m_hWorld.0;
                }
            }
            sample
        }

        fn doc_facts(&self, doc: Doc) -> DocFacts {
            let document = doc.0 as *mut CMapDoc;
            // CMapDoc_Session (`s2sdk.h:389`): the embedding session is the document's
            // address minus the embedded member's offset.
            let session = (doc.0 - offset_of!(CMapDocSession, m_Doc)) as *const CMapDocSession;
            // SAFETY: `doc` passed the identity chain, so the session's `m_Doc` is this very
            // document; `m_Path` is a CUtlString whose `m_pString` the C reads unguarded
            // (`:6245`).
            let path_ptr = unsafe { (*session).m_Doc.m_Path.m_pString };
            let path = if path_ptr.is_null() {
                Vec::new()
            } else {
                // The C hands the raw `char *` to `sb_addjson` (`:6252`), i.e. the whole
                // string. This port caps it at ENTITY_STR_CAP (4096) bytes with `probe_str`,
                // which also cuts at a UTF-8 boundary: a longer path is cut instead of read to
                // its NUL (PROTOCOL.md §8.1).
                probe_text(path_ptr.cast_const(), ENTITY_STR_CAP)
            };
            // SAFETY: `HG_DOCDATA_MANAGER` is the pinned global the C reads; `doc` is live.
            let manager =
                unsafe { *((base() + pins::HG_DOCDATA_MANAGER.value as usize) as *const usize) };
            let dirty = if manager == 0 {
                None
            } else {
                let table = hfn();
                let changed = table.map(|table| {
                    // SAFETY: the verified HFN_CHANGED_SINCE_SAVE function.
                    unsafe {
                        (table.changed_since_save)(manager as *mut c_void, document.cast()) != 0
                    }
                });
                Some(changed.unwrap_or(false))
            };
            let (editable, why) = doc_editable_with(self, doc);
            let owner = <Self as Chain>::owner(self, session as usize);
            let is_new = if owner == 0 {
                false
            } else {
                // SAFETY: the owner is the session that owns this document. The C reads
                // `session->m_bIsNew` as a `u32` (`:6248`); only the byte's truthiness crosses.
                unsafe {
                    flag_byte(
                        ptr::addr_of!((*(owner as *const CHammerEditorSession)).base.m_bIsNew)
                            .cast::<u8>(),
                    )
                }
            };
            DocFacts {
                path,
                dirty,
                active: <Self as Chain>::active_flag(self, doc.0),
                editable,
                why,
                is_new,
                undo: self.undo_info(doc),
            }
        }

        fn undo_info(&self, doc: Doc) -> HistoryInfo {
            let history = <Self as Chain>::undo_history(self, doc.0);
            history_info(history)
        }

        fn entity_read(&self, node: Node) -> Option<EntityRead> {
            let raw = node.0 as *mut CMapNode;
            let element = entity_properties(raw)?;
            let classname = child_string(element, c"classname", 1024);
            let targetname = child_string(element, c"targetname", 1024);
            let origin = node_vec(raw, 0).unwrap_or([0.0, 0.0, 0.0]);
            let angles = node_vec(raw, 1);
            let scales = node_vec(raw, 2);
            let (attrs, truncated) = entity_attrs(element);
            Some(EntityRead {
                classname,
                targetname,
                origin,
                angles,
                scales,
                attrs,
                truncated,
                connections: conn_list(raw),
                children: child_list(raw),
            })
        }

        fn entity_strings(&self, node: Node) -> (Vec<u8>, Vec<u8>, [f32; 3]) {
            let raw = node.0 as *mut CMapNode;
            let mut classname = Vec::new();
            let mut targetname = Vec::new();
            if let Some(element) = entity_properties(raw) {
                classname = child_string(element, c"classname", 512);
                targetname = child_string(element, c"targetname", 512);
            }
            (
                classname,
                targetname,
                node_vec(raw, 0).unwrap_or([0.0, 0.0, 0.0]),
            )
        }
    }

    // -----------------------------------------------------------------------------------------
    // The write primitives (M4 lane HAMMER-W1)
    // -----------------------------------------------------------------------------------------

    use std::ffi::CString;

    use s2sdk_sys::generated::{CUndoHistory, QAngle, Vector};

    use super::bind::HammerFns;
    use super::{
        AttrInfo, ConnSpec, CreateTarget, FilterFields, PrevValue, Write, AT_AT_BOOL, AT_AT_FLOAT,
        AT_AT_INT, AT_AT_STRING, ATTR_TYPE_MASK,
    };

    /// The bound `g_hfn` table. Every write body runs after the document gate, which runs after
    /// `resolve_hammer` bound the table (or after the module was found verified by a read); the C
    /// would call through a null entry, which a caught panic reports instead of crashing.
    fn table() -> HammerFns {
        hfn().expect("cs2-bridge: the hammer function table is bound before any write")
    }

    /// One generated vtable slot the module gate verified (`PIN_SLOTS`). `None` can only mean the
    /// generated table and the pin table disagree; the C would call a null pointer, so this fails
    /// loudly (the drain answers `-32603`).
    fn slot<T>(slot: Option<T>, what: &'static str) -> T {
        match slot {
            Some(slot) => slot,
            None => panic!("cs2-bridge: the verified vtable slot {what} is null"),
        }
    }

    /// `conn_copy_str` (`mcp_bridge.c:3606-3611`): a 256-byte copy that answers `Some` only when
    /// the whole, non-empty value copied (a cut or unreadable value is "does not match").
    fn conn_copy_str(source: *const c_char) -> Option<Vec<u8>> {
        if source.is_null() || !mem_ok(source.cast()) {
            return None;
        }
        let mut buf = [0u8; crate::entity_json::CONN_STR_MAX + 1];
        // SAFETY: `probe_str` re-probes every page boundary and bounds the copy by `buf`.
        let got = unsafe { probe_str(source.cast(), &mut buf) };
        if got == 1 {
            Some(cstr(&buf).to_vec())
        } else {
            None
        }
    }

    impl Write for LiveHammer {
        fn class_exists(&self, cls: &[u8]) -> bool {
            let table = table();
            let Ok(name) = CString::new(cls) else {
                return false;
            };
            // SAFETY: `HG_NODE_FACTORY_REGISTRY` is the pinned global holding the factory registry
            // (`mcp_bridge.c:2100-2101`); the read is the C's `*(void **)(hb + ...)`.
            let registry = unsafe { *((at(pins::HG_NODE_FACTORY_REGISTRY)) as *const *mut c_void) };
            if registry.is_null() {
                return false;
            }
            // SAFETY: the verified HFN_FIND_TYPE_BY_NAME function, called with the registry the C
            // passes and a NUL-terminated class name that outlives the call.
            !unsafe { (table.find_type_by_name)(registry, name.as_ptr()) }.is_null()
        }

        fn create_node(&self, doc: Doc, target: &mut CreateTarget) -> Option<Node> {
            let table = table();
            let Ok(cls) = CString::new(target.cls.clone()) else {
                return None;
            };
            // SAFETY: the verified HFN_CREATE_NODE_BY_NAME function, called exactly as the C does
            // (`mcp_bridge.c:2101`): `doc` passed the identity chain, `cls` is a NUL-terminated
            // name that outlives the call, and the two mutable pointers are the caller's `target`
            // arrays (`float *origin, float *angles`, `s2sdk.h:704`) - they outlive the
            // call and any write-back lands in the storage the reply is built from (`:2122-2129`).
            let node = unsafe {
                (table.create_node_by_name)(
                    doc.0 as *mut c_void,
                    cls.as_ptr(),
                    target.origin.as_mut_ptr(),
                    target.angles.as_mut_ptr(),
                )
            } as *mut CMapNode;
            if node.is_null() {
                return None;
            }
            if target.has_scales {
                let scales = Vector {
                    x: target.scales[0],
                    y: target.scales[1],
                    z: target.scales[2],
                };
                // SAFETY: slot 19 of the created node's vtable (verified by the module gate) is
                // `CMapPoint::SetScales`; the node is the one CreateNodeByName just returned and
                // `scales` outlives the call (`:2110`).
                unsafe {
                    slot(
                        CMapNode::vtable(node).base.SetScales,
                        "CMapPoint::SetScales",
                    )(node.cast(), &scales)
                };
            }
            Some(Node(node as usize))
        }

        fn properties(&self, node: Node) -> Option<usize> {
            entity_properties(node.0 as *mut CMapNode).map(|element| element as usize)
        }

        fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize {
            let table = table();
            // SAFETY: verified HFN_GET_ACTIVE_HISTORY, and `doc` passed the editable gate, whose
            // `active_history`/`undo_history` checks prove this is the document's own stack
            // (`mcp_bridge.c:1834-1839`).
            let hist = unsafe { (table.get_active_history)() };
            // SAFETY: `doc->m_pSelection` is the C's second argument; AddOperation records the
            // group on that history with the C's trailing 0. `label` is `&'static CStr` - the C
            // passes a string literal (`:5398`, `batch_undo_label`), so the pointer Hammer may
            // keep stays valid for the process.
            let selection = unsafe { (*((doc.0) as *const CMapDoc)).m_pSelection };
            unsafe { (table.add_operation)(hist, selection, label.as_ptr(), 0) };
            hist as usize
        }

        fn record_node_tree(&self, hist: usize, node: Node) {
            let table = table();
            // SAFETY: `CMapNode_Undoable` is the `IUndoable` subobject at the generated base offset
            // (`s2sdk.h:498-499`); `hist` is the token `undo_open` returned and the C's
            // recursion flag is 1.
            let undoable = unsafe {
                (node.0 as *const u8)
                    .add(CMapNode::BASE_OFFSET_IUndoable)
                    .cast::<c_void>()
                    .cast_mut()
            };
            unsafe { (table.record_node_tree)(hist as *mut c_void, undoable, 1) };
        }

        fn doc_set_modified(&self, doc: Doc) {
            let table = table();
            // SAFETY: the verified HFN_DOC_SET_MODIFIED function; the C calls it with `(doc, doc)`
            // (`mcp_bridge.c:2449`).
            unsafe { (table.doc_set_modified)(doc.0 as *mut c_void, doc.0 as *mut c_void) };
        }

        fn get_active_history(&self) -> usize {
            let table = table();
            // SAFETY: the verified HFN_GET_ACTIVE_HISTORY function.
            unsafe { (table.get_active_history)() as usize }
        }

        fn get_redo_history(&self, doc: Doc) -> usize {
            let table = table();
            // SAFETY: the verified HFN_GET_REDO_HISTORY function, called with the gated document.
            unsafe { (table.get_redo_history)(doc.0 as *mut c_void) as usize }
        }

        fn history_ok(&self, hist: usize) -> bool {
            // :5762: `obj_is(hist, PIN_HAMMER, HV_HISTORY_VT)` - both stacks share the vtable.
            obj_is(
                hist as *const c_void,
                pins::PIN_HAMMER,
                pins::HV_HISTORY_VT.value as u32,
            )
        }

        fn history_info(&self, hist: usize) -> HistoryInfo {
            history_info(hist)
        }

        fn undo_pop(&self, hist: usize) {
            let hist = hist as *mut CUndoHistory;
            // SAFETY: the history passed `obj_is(HV_HISTORY_VT)`; `CUndoHistory` is
            // `CMapDocHistory`'s base at offset 0 and its slot 2 is the pinned
            // `CMapDocHistory::Undo` (the apply-and-pop), which the C calls the same way
            // (`mcp_bridge.c:5781`).
            unsafe {
                slot(CUndoHistory::vtable(hist).Undo, "CUndoHistory::Undo")(hist)
            };
        }

        fn node_locked(&self, node: Node) -> bool {
            let table = table();
            // SAFETY: the verified HFN_NODE_IS_LOCKED function, called with the C's trailing 0.
            unsafe { (table.node_is_locked)(node.0 as *mut c_void, 0) != 0 }
        }

        fn delete_node(&self, doc: Doc, node: Node) {
            let table = table();
            // SAFETY: the verified HFN_DELETE_NODE function, with the gated document and a node
            // the caller validated (exists, not world/other, unlocked).
            unsafe { (table.delete_node)(doc.0 as *mut c_void, node.0 as *mut c_void) };
        }

        fn set_origin(&self, node: Node, value: [f32; 3]) {
            let node = node.0 as *mut CMapNode;
            let vector = Vector {
                x: value[0],
                y: value[1],
                z: value[2],
            };
            // SAFETY: slot 17 of the node's verified vtable is `CMapPoint::SetOrigin`; `node` was
            // checked to be an entity or mesh and `vector` outlives the call (`:5589`).
            unsafe {
                slot(CMapNode::vtable(node).base.SetOrigin, "CMapPoint::SetOrigin")(
                    node.cast(),
                    &vector,
                )
            };
        }

        fn set_angles(&self, node: Node, value: [f32; 3]) {
            let node = node.0 as *mut CMapNode;
            let angles = QAngle {
                x: value[0],
                y: value[1],
                z: value[2],
            };
            // SAFETY: slot 18 of the node's verified vtable is `CMapPoint::SetAngles` (`:5590`).
            unsafe {
                slot(CMapNode::vtable(node).base.SetAngles, "CMapPoint::SetAngles")(
                    node.cast(),
                    &angles,
                )
            };
        }

        fn set_scales(&self, node: Node, value: [f32; 3]) {
            let node = node.0 as *mut CMapNode;
            let scales = Vector {
                x: value[0],
                y: value[1],
                z: value[2],
            };
            // SAFETY: slot 19 of the node's verified vtable is `CMapPoint::SetScales` (`:5588`).
            unsafe {
                slot(CMapNode::vtable(node).base.SetScales, "CMapPoint::SetScales")(
                    node.cast(),
                    &scales,
                )
            };
        }

        fn angles(&self, node: Node) -> Option<[f32; 3]> {
            node_vec(node.0 as *mut CMapNode, 1)
        }

        fn scales(&self, node: Node) -> Option<[f32; 3]> {
            node_vec(node.0 as *mut CMapNode, 2)
        }

        fn attr_previous(&self, element: usize, name: &[u8]) -> PrevValue {
            // The C's previous pass interns, FindAttribute's and then reads the value
            // (`mcp_bridge.c:2320-2324` + `kv_previous_json`, `:2132-2147`).
            let info = attr_info_read(element, name);
            let attr = info.attr as *mut CDmAttribute;
            if attr.is_null() {
                return PrevValue::Null;
            }
            let last = (attr as *const u8).wrapping_add(core::mem::size_of::<CDmAttribute>() - 1);
            if !mem_ok(attr.cast()) || !mem_ok(last.cast()) {
                return PrevValue::Null;
            }
            // SAFETY: both ends of the attribute record were just proven readable.
            let kind = unsafe { (*attr).m_nFlags } & ATTR_TYPE_MASK;
            let typed = |attr: *mut CDmAttribute| -> Option<*const u8> {
                // SAFETY: the caller took the attribute from the element's readable chain.
                let storage = unsafe { (*attr).m_pData } as *const u8;
                (mem_ok(storage.cast()) && mem_ok(storage.wrapping_add(3).cast())).then_some(storage)
            };
            match kind {
                AT_AT_STRING => {
                    // SAFETY: `attr_cstr` proves every pointer it follows.
                    let Some(text) = attr_cstr(attr) else {
                        return PrevValue::Null;
                    };
                    let mut buf = vec![0u8; crate::entity_json::KV_VALUE_MAX + 1];
                    // SAFETY: `probe_str` re-probes every page boundary.
                    let got = unsafe { probe_str(text.cast(), &mut buf) };
                    if got == 1 {
                        PrevValue::Str(cstr(&buf).to_vec())
                    } else if got < 0 {
                        // :2143-2144: a cut value is null AND listed in previousTruncated.
                        PrevValue::Cut
                    } else {
                        PrevValue::Null
                    }
                }
                AT_AT_INT => match typed(attr) {
                    // SAFETY: the four value bytes were just proven readable.
                    Some(storage) => PrevValue::Int(unsafe {
                        (storage as *const i32).read_unaligned()
                    }),
                    None => PrevValue::Null,
                },
                AT_AT_FLOAT => match typed(attr) {
                    // SAFETY: the four value bytes were just proven readable.
                    Some(storage) => PrevValue::Float(unsafe {
                        (storage as *const f32).read_unaligned()
                    }),
                    None => PrevValue::Null,
                },
                AT_AT_BOOL => match typed(attr) {
                    // SAFETY: the four value bytes were just proven readable. The C prints
                    // `*v ? "true" : "false"` from an `int *`; only the truthiness crosses.
                    Some(storage) => {
                        PrevValue::Bool(unsafe { (storage as *const i32).read_unaligned() } != 0)
                    }
                    None => PrevValue::Null,
                },
                _ => PrevValue::Null,
            }
        }

        fn attr_info(&self, element: usize, name: &[u8]) -> AttrInfo {
            attr_info_read(element, name)
        }

        fn case_variant(&self, element: usize, name: &[u8]) -> Option<Vec<u8>> {
            // `kv_case_variant` (`mcp_bridge.c:2029-2037`): the entity's attribute that matches
            // `name` case-insensitively but NOT exactly.
            let element = element as *mut CDmElement;
            // SAFETY: the caller's identity-checked properties element; `addr_of!` computes the
            // first attribute's address only.
            let mut attr = unsafe { ptr::addr_of!((*element).m_pAttributes).read_unaligned() };
            for _guard in 0..4096 {
                if attr.is_null() {
                    return None;
                }
                let last =
                    (attr as *const u8).wrapping_add(core::mem::size_of::<CDmAttribute>() - 1);
                if !mem_ok(attr.cast()) || !mem_ok(last.cast()) {
                    return None;
                }
                // SAFETY: the attribute was proven readable.
                let attr_name = unsafe { (*attr).m_Name.m_pString };
                if !attr_name.is_null() && mem_ok(attr_name.cast()) {
                    // SAFETY: the first byte was proven readable; `name_eq_ci` re-probes every
                    // byte it walks and stops at the first difference or the shared NUL.
                    let (exact, insensitive) = unsafe { name_eq_ci(attr_name, name) };
                    if !exact && insensitive {
                        // :2021: the C hands the pointer to `probe_str` in a 96-byte buffer.
                        let mut buf = [0u8; 96];
                        // SAFETY: `probe_str` re-probes every page boundary.
                        let got = unsafe { probe_str(attr_name.cast(), &mut buf) };
                        return Some(if got == 0 {
                            Vec::new()
                        } else {
                            cstr(&buf).to_vec()
                        });
                    }
                }
                // SAFETY: the attribute was proven readable.
                attr = unsafe { (*attr).m_pNext };
            }
            None
        }

        fn set_key_value(&self, node: Node, name: &[u8], value: &[u8]) {
            let table = table();
            // The C passes plain C strings (the name kv_next produced and the token text); both
            // are NUL-free here because they were cut at their first NUL already.
            let key = CString::new(name).expect("a kv name carries no interior NUL");
            let text = CString::new(value).expect("a kv value carries no interior NUL");
            // SAFETY: the verified HFN_SET_KEY_VALUE function (`CMapGameDataNode::SetKeyValue`);
            // `node` came from create/geo_entity_node and both strings outlive the call.
            unsafe { (table.set_key_value)(node.0 as *mut c_void, key.as_ptr(), text.as_ptr()) };
        }

        fn set_value_int(&self, attr: usize, value: i32) {
            let table = table();
            // SAFETY: the verified HFN_SET_INT function, with the attribute pointer the caller's
            // `attr_info` just resolved from the live element.
            unsafe { (table.set_value_int)(attr as *mut c_void, &value) };
        }

        fn set_value_float(&self, attr: usize, value: f32) {
            let table = table();
            // SAFETY: the verified HFN_SET_FLOAT function, same attribute contract as above.
            unsafe { (table.set_value_float)(attr as *mut c_void, &value) };
        }

        fn conn_count(&self, node: Node) -> Option<usize> {
            let (count, _) = conn_vec(node.0 as *mut CMapNode)?;
            Some(count as usize)
        }

        fn conn_entry(&self, node: Node, index: usize) -> Option<Conn> {
            let (count, data) = conn_vec(node.0 as *mut CMapNode)?;
            if index >= count as usize {
                return None;
            }
            // SAFETY: `conn_vec` gave the live vector and `index` is inside its count; `read_conn`
            // proves every byte of the entry before copying it.
            unsafe { read_conn(data, index) }
        }

        fn conn_filter_fields(&self, node: Node, index: usize) -> Option<FilterFields> {
            let (count, data) = conn_vec(node.0 as *mut CMapNode)?;
            if index >= count as usize {
                return None;
            }
            let entry = data.wrapping_add(core::mem::size_of::<MapEntityConnection_t>() * index);
            let last = entry.wrapping_add(core::mem::size_of::<MapEntityConnection_t>() - 1);
            // `conn_entry_ok` (`:3573-3576`): an unreadable entry never matches.
            if !mem_ok(entry.cast()) || !mem_ok(last.cast()) {
                return None;
            }
            // SAFETY: the whole record was just proven readable, and `read_unaligned` copies it
            // out by value (no reference into the game vector).
            let connection = unsafe { entry.cast::<MapEntityConnection_t>().read_unaligned() };
            Some(FilterFields {
                output: conn_copy_str(connection.m_unk002.m_pString),
                target: conn_copy_str(connection.m_unk003.m_pString),
                input: conn_copy_str(connection.m_unk004.m_pString),
            })
        }

        fn add_connection(&self, node: Node, spec: &ConnSpec) -> (bool, Option<i32>) {
            let table = table();
            // SAFETY: `MapEntityConnection_t` is the C's 80-byte `S2ConnectionSrc` (all members
            // are integers, pointers or byte arrays), so an all-zero record is valid; the C
            // zeroes it the same way before filling it in (`mcp_bridge.c:3758-3762`).
            let mut src: MapEntityConnection_t = unsafe { core::mem::zeroed() };
            // The four CUtlString members point at `spec`'s validated, NUL-terminated buffers.
            // AddConnection copies them (strlen + SetDirect + a KV3 copy) before it returns, and
            // `spec` outlives this call, exactly like the C's `ConnSpec` - the callee only reads
            // through these pointers.
            src.m_unk002.m_pString = spec.output().as_ptr().cast_mut().cast();
            src.m_unk003.m_pString = spec.target().as_ptr().cast_mut().cast();
            src.m_unk004.m_pString = spec.input().as_ptr().cast_mut().cast();
            src.m_unk005.m_pString = spec.param().as_ptr().cast_mut().cast();
            src.m_unk006 = 7; // :3764: the UI default and every saved Valve connection
            // SAFETY: `m_unk007` is the 16-byte KeyValues3 `S2ConnectionSrc` carries; Kv3Init
            // initializes it in place (type 1, subtype 8) and will be paired with Kv3Free below.
            unsafe { (table.kv3_init)(ptr::addr_of_mut!(src.m_unk007).cast(), 1, 8) };
            src.m_unk008 = spec.delay.to_bits(); // :3765: the field is the float's bits
            src.m_unk009 = spec.times_to_fire;
            let mut id: u32 = 4294967295; // the C's `u32 cid = 4294967295u`
            // SAFETY: the verified HFN_ADD_CONNECTION function; `id` is its out-parameter and
            // `src` a fully initialized record that outlives the call.
            unsafe {
                (table.add_connection)(node.0 as *mut c_void, &mut id, ptr::addr_of!(src).cast())
            };
            // SAFETY: pairs the Kv3Init above; `src` is still alive.
            unsafe { (table.kv3_free)(ptr::addr_of_mut!(src.m_unk007).cast()) };
            let landed = id != 4294967295 && (id & 65535) == 0; // :3777
            (landed, if landed { Some((id >> 16) as i32) } else { None })
        }

        fn remove_connection(&self, node: Node, index: i32) -> bool {
            let table = table();
            // SAFETY: the verified HFN_REMOVE_CONNECTION function; the id is `index << 16` and its
            // answer is the C's truthiness (`:3960`).
            unsafe { (table.remove_connection)(node.0 as *mut c_void, (index as u32) << 16) != 0 }
        }
    }

    /// `seq` / `ci_seq` against a game-owned C string (`mcp_bridge.c:157-180`): compares at most
    /// `name.len() + 1` bytes and answers `(exact, case-insensitive)`.
    ///
    /// # Safety
    ///
    /// `text` is a game pointer whose first byte the caller proved readable; every further byte is
    /// re-probed here, so an unreadable name answers `(false, false)` instead of faulting.
    unsafe fn name_eq_ci(text: *const c_char, name: &[u8]) -> (bool, bool) {
        let mut exact = true;
        let mut insensitive = true;
        for (index, &want) in name.iter().enumerate() {
            let at = text.cast::<u8>().wrapping_add(index);
            if !mem_ok(at.cast()) {
                return (false, false);
            }
            // SAFETY: `mem_ok` just proved this byte readable.
            let got = unsafe { at.read() };
            if got != want {
                exact = false;
            }
            if !got.eq_ignore_ascii_case(&want) {
                insensitive = false;
            }
            if !exact && !insensitive {
                return (false, false);
            }
        }
        let at = text.cast::<u8>().wrapping_add(name.len());
        if !mem_ok(at.cast()) {
            return (false, false);
        }
        // SAFETY: the byte after the name was just proven readable; equality needs the shared NUL.
        let end = unsafe { at.read() } == 0;
        (exact && end, insensitive && end)
    }

    /// The write pass's per-key reads (`mcp_bridge.c:2365-2372`): `dm_intern`, `FindAttribute`,
    /// then `typeFlags & S2_ATTR_TYPE_MASK` once the record is proven readable.
    fn attr_info_read(element: usize, name: &[u8]) -> AttrInfo {
        let mut info = AttrInfo {
            symbol: false,
            attr: 0,
            present: false,
            readable: false,
            kind: -1,
        };
        let table = table();
        let Ok(text) = CString::new(name) else {
            return info;
        };
        let sym = dm_intern(&text);
        if sym.m_pString.is_null() {
            return info;
        }
        info.symbol = true;
        // SAFETY: `element` is the caller's identity-checked properties element and
        // `HFN_FIND_ATTRIBUTE` is the verified function.
        let attr = unsafe {
            (table.find_attribute)(element as *mut c_void, &sym) as *mut CDmAttribute
        };
        info.attr = attr as usize;
        info.present = !attr.is_null();
        if !info.present {
            return info;
        }
        let last = (attr as *const u8).wrapping_add(core::mem::size_of::<CDmAttribute>() - 1);
        info.readable = mem_ok(attr.cast()) && mem_ok(last.cast());
        if info.readable {
            // SAFETY: both ends of the attribute record were just proven readable.
            info.kind = (unsafe { (*attr).m_nFlags } & ATTR_TYPE_MASK) as i32;
        }
        info
    }

    // `log` and `modules` are the reader's own imports (`resolve_hammer`, `data_model`).
}

/// The production writer on the Windows tools build: the same [`win::LiveHammer`] object, whose
/// state is the module's pins, the bound `g_hfn` table and the fingerprint/handle statics.
///
/// The mesh lane (`ops::mesh`) composes the same object: its host is this `&dyn Write` plus the
/// mesh-only primitives, so `undo_open`/`record_node_tree`/`SetAngles`/`SetScales` are this one
/// implementation, not a copy.
#[cfg(windows)]
pub(crate) fn game_write() -> &'static dyn Write {
    &win::LIVE
}

/// Non-Windows: there is no Hammer module, so the `-32017` gate answers before a write body runs.
/// Anything that still reaches one fails loudly instead of pretending it wrote.
#[cfg(not(windows))]
pub(crate) fn game_write() -> &'static dyn Write {
    &NoHammer
}

/// The production reader on the Windows tools build, a loud refusal everywhere else.
#[cfg(windows)]
pub(crate) fn game() -> &'static dyn Hammer {
    &win::LIVE
}

/// Non-Windows: there is no Hammer module (the tools DLL only ever runs on Windows), so the
/// method gate answers `-32017` before a body starts. Anything that still reaches a read fails
/// loudly instead of inventing an answer.
#[cfg(not(windows))]
pub(crate) fn game() -> &'static dyn Hammer {
    &NoHammer
}

/// The absent-platform reader.
#[cfg(not(windows))]
struct NoHammer;

#[cfg(not(windows))]
impl Chain for NoHammer {
    fn module_reason(&self) -> Option<&'static str> {
        Some("hammer_not_loaded")
    }
    fn app(&self) -> Option<usize> {
        None
    }
    fn session(&self, _app: usize) -> Option<usize> {
        None
    }
    fn doc_session(&self, _session: usize) -> Option<usize> {
        None
    }
    fn owner(&self, _doc_session: usize) -> usize {
        0
    }
    fn doc_of(&self, _doc_session: usize) -> usize {
        0
    }
    fn doc_backptr(&self, _doc: usize) -> usize {
        0
    }
    fn is_of(&self, _object: usize, _vtable: u32) -> bool {
        false
    }
    fn root(&self, _doc: usize) -> usize {
        0
    }
    fn active_flag(&self, _doc: usize) -> bool {
        false
    }
    fn selection(&self, _doc: usize) -> usize {
        0
    }
    fn scope_depth(&self, _doc: usize) -> u32 {
        0
    }
    fn undo_history(&self, _doc: usize) -> usize {
        0
    }
    fn active_history(&self) -> usize {
        0
    }
}

#[cfg(not(windows))]
fn no_hammer(what: &str) -> ! {
    panic!("cs2-bridge: {what} needs the Windows tools build (Hammer is a Windows module)")
}

#[cfg(not(windows))]
impl Nodes for NoHammer {
    fn children(&self, _node: Node) -> Option<Vec<Node>> {
        no_hammer("a Hammer node's children")
    }
    fn class(&self, _node: Node) -> &'static str {
        no_hammer("a Hammer node's class")
    }
    fn id(&self, _node: Node) -> Option<i32> {
        no_hammer("a Hammer node's id")
    }
    fn origin(&self, _node: Node) -> Option<[f32; 3]> {
        no_hammer("a Hammer node's origin")
    }
    fn mesh_slots(&self, _node: Node) -> Option<(i32, i32)> {
        no_hammer("a Hammer mesh's component counts")
    }
    fn world(&self, _doc: Doc) -> Option<Node> {
        no_hammer("a Hammer document's world node")
    }
}

#[cfg(not(windows))]
impl Hammer for NoHammer {
    fn active_doc(&self) -> Result<Doc, &'static str> {
        active_doc_with(self)
    }
    fn fp_sample(&self) -> FpSample {
        FpSample::default()
    }
    fn doc_facts(&self, _doc: Doc) -> DocFacts {
        no_hammer("doc.active's document fields")
    }
    fn undo_info(&self, _doc: Doc) -> HistoryInfo {
        HistoryInfo::default()
    }
    fn entity_read(&self, _node: Node) -> Option<EntityRead> {
        no_hammer("entity.get's document read")
    }
    fn entity_strings(&self, _node: Node) -> (Vec<u8>, Vec<u8>, [f32; 3]) {
        no_hammer("entity.find's document read")
    }
}

#[cfg(not(windows))]
impl Write for NoHammer {
    fn class_exists(&self, _cls: &[u8]) -> bool {
        no_hammer("entity.create's class lookup")
    }
    fn create_node(&self, _doc: Doc, _target: &mut CreateTarget) -> Option<Node> {
        no_hammer("entity.create's node")
    }
    fn properties(&self, _node: Node) -> Option<usize> {
        no_hammer("an entity's properties element")
    }
    fn undo_open(&self, _doc: Doc, _label: &'static CStr) -> usize {
        no_hammer("an undo group")
    }
    fn record_node_tree(&self, _hist: usize, _node: Node) {
        no_hammer("an undo record")
    }
    fn doc_set_modified(&self, _doc: Doc) {
        no_hammer("the document's modified flag")
    }
    fn get_active_history(&self) -> usize {
        no_hammer("the active undo history")
    }
    fn get_redo_history(&self, _doc: Doc) -> usize {
        no_hammer("the redo history")
    }
    fn history_ok(&self, _hist: usize) -> bool {
        no_hammer("an undo history's class")
    }
    fn history_info(&self, _hist: usize) -> HistoryInfo {
        no_hammer("an undo history's top group")
    }
    fn undo_pop(&self, _hist: usize) {
        no_hammer("an undo pop")
    }
    fn node_locked(&self, _node: Node) -> bool {
        no_hammer("a node's lock flag")
    }
    fn delete_node(&self, _doc: Doc, _node: Node) {
        no_hammer("node.delete's document write")
    }
    fn set_origin(&self, _node: Node, _value: [f32; 3]) {
        no_hammer("node.setTransform's origin write")
    }
    fn set_angles(&self, _node: Node, _value: [f32; 3]) {
        no_hammer("node.setTransform's angles write")
    }
    fn set_scales(&self, _node: Node, _value: [f32; 3]) {
        no_hammer("node.setTransform's scales write")
    }
    fn angles(&self, _node: Node) -> Option<[f32; 3]> {
        no_hammer("a Hammer node's angles")
    }
    fn scales(&self, _node: Node) -> Option<[f32; 3]> {
        no_hammer("a Hammer node's scales")
    }
    fn attr_previous(&self, _element: usize, _name: &[u8]) -> PrevValue {
        no_hammer("a keyvalue's previous value")
    }
    fn attr_info(&self, _element: usize, _name: &[u8]) -> AttrInfo {
        no_hammer("a keyvalue's attribute")
    }
    fn case_variant(&self, _element: usize, _name: &[u8]) -> Option<Vec<u8>> {
        no_hammer("a keyvalue's case variant")
    }
    fn set_key_value(&self, _node: Node, _name: &[u8], _value: &[u8]) {
        no_hammer("a keyvalue write")
    }
    fn set_value_int(&self, _attr: usize, _value: i32) {
        no_hammer("an int attribute write")
    }
    fn set_value_float(&self, _attr: usize, _value: f32) {
        no_hammer("a float attribute write")
    }
    fn conn_count(&self, _node: Node) -> Option<usize> {
        no_hammer("an entity's connection count")
    }
    fn conn_entry(&self, _node: Node, _index: usize) -> Option<Conn> {
        no_hammer("an entity's connection")
    }
    fn conn_filter_fields(&self, _node: Node, _index: usize) -> Option<FilterFields> {
        no_hammer("an entity's connection strings")
    }
    fn add_connection(&self, _node: Node, _spec: &ConnSpec) -> (bool, Option<i32>) {
        no_hammer("an entity's connection write")
    }
    fn remove_connection(&self, _node: Node, _index: i32) -> bool {
        no_hammer("an entity's connection removal")
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // each case names the one hop that fails
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The build string every envelope carries (`control::BRIDGE_BUILD`, the C's `BRIDGE_BUILD`).
    const BUILD: &str = crate::BRIDGE_BUILD;

    /// The game's `bool` returns leave junk in the upper bits of EAX/RAX; only AL counts. A
    /// fake returning a full `u64` with a clean AL stands in for the game (Win64 and SysV
    /// both return in RAX), and the pinned fn types must read it as clean.
    #[cfg(all(test, target_arch = "x86_64"))]
    mod bool_abi {
        use super::*;

        extern "C" fn fake_clean(_: *mut c_void, _: *mut c_void) -> u64 {
            0xDEAD_BE00
        }
        extern "C" fn fake_dirty(_: *mut c_void, _: *mut c_void) -> u64 {
            0x101
        }
        extern "C" fn fake_dep_clean(_: *mut c_void, _: i32) -> u64 {
            0x100
        }
        extern "C" fn fake_dep_dirty(_: *mut c_void, _: i32) -> u64 {
            1
        }

        #[test]
        fn changed_since_save_reads_al_only() {
            let p = std::ptr::null_mut::<c_void>();
            // SAFETY: same calling convention and arity; only the return width is faked.
            let clean: FnChangedSinceSave = unsafe { std::mem::transmute(fake_clean as *const ()) };
            let dirty: FnChangedSinceSave = unsafe { std::mem::transmute(fake_dirty as *const ()) };
            assert!(unsafe { clean(p, p) } == 0, "junk upper bits with AL=0 is clean");
            assert!(unsafe { dirty(p, p) } != 0, "AL=1 is dirty");
        }

        #[test]
        fn doc_or_dependency_changed_reads_al_only() {
            let p = std::ptr::null_mut::<c_void>();
            // SAFETY: as above.
            let clean: FnDocOrDependencyChanged =
                unsafe { std::mem::transmute(fake_dep_clean as *const ()) };
            let dirty: FnDocOrDependencyChanged =
                unsafe { std::mem::transmute(fake_dep_dirty as *const ()) };
            assert!(unsafe { clean(p, 0) } == 0, "0x100 is clean");
            assert!(unsafe { dirty(p, 0) } != 0, "1 is dirty");
        }
    }


    // -----------------------------------------------------------------------------------------
    // A fabricated Hammer: the identity chain and one-level tree, no game memory. The builders
    // and every parameter gate are driven through it.
    // -----------------------------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct FakeNode {
        class: &'static str,
        id: Option<i32>,
        origin: Option<[f32; 3]>,
        mesh: Option<(i32, i32)>,
        children: Option<Vec<usize>>,
        entity: Option<EntityRead>,
        strings: Option<(Vec<u8>, Vec<u8>)>,
    }

    struct Fake {
        module_reason: Option<&'static str>,
        app: Option<usize>,
        session: Option<usize>,
        doc_session: Option<usize>,
        owner: usize,
        doc: usize,
        doc_backptr: usize,
        bad: Vec<usize>,
        root: usize,
        active: bool,
        selection: usize,
        scope: u32,
        undo: usize,
        active_hist: usize,
        world: Option<usize>,
        nodes: BTreeMap<usize, FakeNode>,
        script: std::cell::RefCell<WScript>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                module_reason: None,
                app: Some(1),
                session: Some(2),
                doc_session: Some(3),
                owner: 2,
                doc: 4,
                doc_backptr: 3,
                bad: Vec::new(),
                root: 5,
                active: true,
                selection: 6,
                scope: 0,
                undo: 7,
                active_hist: 7,
                world: Some(10),
                nodes: BTreeMap::new(),
                script: std::cell::RefCell::new(WScript::default()),
            }
        }
    }

    impl Fake {
        fn with_node(mut self, handle: usize, node: FakeNode) -> Self {
            self.nodes.insert(handle, node);
            self
        }

        fn reason(&self, params: Option<&str>) -> Result<(), OpErr> {
            let mut buf = vec![0u8; 4096];
            let mut out = Sb::new(&mut buf);
            geo_read(self, params, &mut out, |hammer, doc, params, out| {
                op_node_list(hammer, doc, params, out)
            })
        }
    }

    impl Chain for Fake {
        fn module_reason(&self) -> Option<&'static str> {
            self.module_reason
        }
        fn app(&self) -> Option<usize> {
            self.app
        }
        fn session(&self, _app: usize) -> Option<usize> {
            self.session
        }
        fn doc_session(&self, _session: usize) -> Option<usize> {
            self.doc_session
        }
        fn owner(&self, _doc_session: usize) -> usize {
            self.owner
        }
        fn doc_of(&self, _doc_session: usize) -> usize {
            self.doc
        }
        fn doc_backptr(&self, _doc: usize) -> usize {
            self.doc_backptr
        }
        fn is_of(&self, object: usize, vtable: u32) -> bool {
            if object == 0 || self.bad.contains(&object) {
                return false;
            }
            // A node the fabricated tree holds answers by its class, the same predicate
            // `node_class` uses (`mcp_bridge.c:3297-3306`); every other object (the session, the
            // document, the history) is the caller's identity-checked one.
            match self.nodes.get(&object) {
                Some(node) if vtable == pins::HV_MAPENTITY_VT.value as u32 => node.class == "entity",
                Some(node) if vtable == pins::HV_MAPMESH_VT.value as u32 => node.class == "mesh",
                Some(_) => false,
                None => true,
            }
        }
        fn root(&self, _doc: usize) -> usize {
            self.root
        }
        fn active_flag(&self, _doc: usize) -> bool {
            self.active
        }
        fn selection(&self, _doc: usize) -> usize {
            self.selection
        }
        fn scope_depth(&self, _doc: usize) -> u32 {
            self.scope
        }
        fn undo_history(&self, _doc: usize) -> usize {
            self.undo
        }
        fn active_history(&self) -> usize {
            self.active_hist
        }
    }

    impl Nodes for Fake {
        fn children(&self, node: Node) -> Option<Vec<Node>> {
            self.nodes
                .get(&node.0)?
                .children
                .clone()
                .map(|kids| kids.into_iter().map(Node).collect())
        }
        fn class(&self, node: Node) -> &'static str {
            self.nodes.get(&node.0).map_or("other", |node| node.class)
        }
        fn id(&self, node: Node) -> Option<i32> {
            self.nodes.get(&node.0).and_then(|node| node.id)
        }
        fn origin(&self, node: Node) -> Option<[f32; 3]> {
            self.nodes.get(&node.0).and_then(|node| node.origin)
        }
        fn mesh_slots(&self, node: Node) -> Option<(i32, i32)> {
            self.nodes.get(&node.0).and_then(|node| node.mesh)
        }
        fn world(&self, _doc: Doc) -> Option<Node> {
            self.world.map(Node)
        }
    }

    impl Hammer for Fake {
        fn active_doc(&self) -> Result<Doc, &'static str> {
            active_doc_with(self)
        }
        fn fp_sample(&self) -> FpSample {
            FpSample::default()
        }
        fn doc_facts(&self, doc: Doc) -> DocFacts {
            let (editable, why) = doc_editable_with(self, doc);
            DocFacts {
                path: b"maps/a.vmap".to_vec(),
                dirty: Some(false),
                active: self.active,
                editable,
                why,
                is_new: false,
                undo: HistoryInfo::default(),
            }
        }
        fn undo_info(&self, _doc: Doc) -> HistoryInfo {
            HistoryInfo::default()
        }
        fn entity_read(&self, node: Node) -> Option<EntityRead> {
            self.nodes.get(&node.0)?.entity.clone()
        }
        fn entity_strings(&self, node: Node) -> (Vec<u8>, Vec<u8>, [f32; 3]) {
            let Some(node) = self.nodes.get(&node.0) else {
                return (Vec::new(), Vec::new(), [0.0, 0.0, 0.0]);
            };
            let (classname, targetname) = node.strings.clone().unwrap_or_default();
            (classname, targetname, node.origin.unwrap_or([0.0, 0.0, 0.0]))
        }
    }

    fn run_node_list(fake: &Fake, params: &str) -> String {
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        op_node_list(fake, Doc(4), Some(params), &mut out).expect("no error");
        String::from_utf8_lossy(out.as_cstr()).into_owned()
    }

    fn run_entity_find(fake: &Fake, params: &str) -> String {
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        op_entity_find(fake, Doc(4), Some(params), &mut out).expect("no error");
        String::from_utf8_lossy(out.as_cstr()).into_owned()
    }

    fn run_entity_get(fake: &Fake, params: Option<&str>) -> Result<String, OpErr> {
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        op_entity_get(fake, Doc(4), params, &mut out)?;
        Ok(String::from_utf8_lossy(out.as_cstr()).into_owned())
    }

    // -----------------------------------------------------------------------------------------
    // `probe_str` (`mcp_bridge.c:1971-1985`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn probe_str_copies_or_cuts_a_game_string() {
        let fits = c"hello";
        let mut out = [0u8; 8];
        // SAFETY: a local byte string.
        assert_eq!(unsafe { probe_str(fits.as_ptr().cast(), &mut out) }, 1);
        assert_eq!(&out[..6], b"hello\0");

        // Exactly cap-1 bytes + NUL: the C's tail check answers 1 (`:1982`).
        let mut out = [0u8; 4];
        // SAFETY: a local byte string.
        assert_eq!(unsafe { probe_str(c"abc".as_ptr().cast(), &mut out) }, 1);
        assert_eq!(&out[..4], b"abc\0");

        // Longer: cut at cap-1, `-1`, prefix left (`:1984-1985`).
        let mut out = [0u8; 4];
        // SAFETY: a local byte string.
        assert_eq!(unsafe { probe_str(c"abcdef".as_ptr().cast(), &mut out) }, -1);
        assert_eq!(&out[..4], b"abc\0");

        // A cut never lands inside a UTF-8 sequence (`:1984`).
        let mut out = [0u8; 3];
        // SAFETY: a local byte string ("a" + U+00E9).
        assert_eq!(unsafe { probe_str(c"a\xC3\xA9".as_ptr().cast(), &mut out) }, -1);
        assert_eq!(&out[..2], b"a\0");

        // Null and unreadable answer 0 with an empty buffer (`:1974`).
        let mut out = [7u8; 4];
        // SAFETY: null is handled before any read.
        assert_eq!(unsafe { probe_str(core::ptr::null(), &mut out) }, 0);
        assert_eq!(out[0], 0);
        // SAFETY: the first page is unmapped; mem_ok reports it instead of faulting.
        assert_eq!(unsafe { probe_str(0x10 as *const u8, &mut out) }, 0);

        // cap < 2 is the C's early 0.
        let mut out = [0u8; 1];
        // SAFETY: a local byte string.
        assert_eq!(unsafe { probe_str(fits.as_ptr().cast(), &mut out) }, 0);
    }

    // -----------------------------------------------------------------------------------------
    // The C's early-exit string compare and its flag bytes
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_attribute_name_compare_exits_at_the_first_difference() {
        // `seq(a->name, name)` (`mcp_bridge.c:3288`): equality up to the shared NUL.
        let name = c"nodeID";
        // SAFETY: every pointer here is a local byte string.
        assert!(unsafe { name_is(c"nodeID".as_ptr().cast(), name) });
        // SAFETY: local byte strings.
        assert!(!unsafe { name_is(c"nodeIX".as_ptr().cast(), name) });
        // SAFETY: local byte strings.
        assert!(!unsafe { name_is(c"node".as_ptr().cast(), name) });
        // SAFETY: local byte strings.
        assert!(!unsafe { name_is(c"nodeIDX".as_ptr().cast(), name) });
        // The first byte differs, so the comparison must stop there: this buffer is one byte
        // with no terminator, and reading any further would be out of bounds.
        // SAFETY: the first (only) byte is local and readable.
        assert!(!unsafe { name_is(b"x".as_ptr(), name) });
    }

    #[test]
    fn a_game_flag_byte_is_true_for_any_nonzero_value() {
        // `doc->m_bActive` / `session->m_bIsNew` read as `u32` in the C (`:6246`, `:6248`).
        let bytes = [0u8, 1, 2, 0xFF];
        // SAFETY: local bytes.
        assert!(!unsafe { flag_byte(bytes.as_ptr()) });
        for index in 1..bytes.len() {
            // SAFETY: `index` is in bounds of the local array.
            assert!(unsafe { flag_byte(bytes.as_ptr().add(index)) }, "{index}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // `active_doc`'s reason chain (`mcp_bridge.c:1713-1735`)
    // -----------------------------------------------------------------------------------------
    #[test]
    fn active_doc_reports_each_hop_reason_in_order() {
        assert_eq!(active_doc_with(&Fake::default()), Ok(Doc(4)));

        let mut fake = Fake::default();
        fake.module_reason = Some("hammer_verifying");
        assert_eq!(active_doc_with(&fake), Err("hammer_verifying"));

        let mut fake = Fake::default();
        fake.app = None;
        assert_eq!(active_doc_with(&fake), Err("hammer_unavailable"));

        let mut fake = Fake::default();
        fake.session = None;
        assert_eq!(active_doc_with(&fake), Err("no_session"));

        let mut fake = Fake::default();
        fake.bad.push(2); // the session object
        assert_eq!(active_doc_with(&fake), Err("bad_session"));

        let mut fake = Fake::default();
        fake.doc_session = None;
        assert_eq!(active_doc_with(&fake), Err("no_document"));

        let mut fake = Fake::default();
        fake.bad.push(3); // the doc session
        assert_eq!(active_doc_with(&fake), Err("bad_doc_session"));

        let mut fake = Fake::default();
        fake.owner = 99;
        assert_eq!(active_doc_with(&fake), Err("doc_session_backptr"));

        let mut fake = Fake::default();
        fake.bad.push(4); // the document
        assert_eq!(active_doc_with(&fake), Err("bad_doc"));

        let mut fake = Fake::default();
        fake.doc_backptr = 99;
        assert_eq!(active_doc_with(&fake), Err("doc_backptr"));
    }

    // -----------------------------------------------------------------------------------------
    // `doc_editable`'s preconditions (`mcp_bridge.c:1780-1802`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn doc_editable_reports_each_precondition_in_order() {
        assert_eq!(doc_editable_with(&Fake::default(), Doc(4)), (true, "ok"));

        let mut fake = Fake::default();
        fake.root = 0;
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "no_root"));

        let mut fake = Fake::default();
        fake.bad.push(5);
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "bad_root"));

        let mut fake = Fake::default();
        fake.active = false;
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "inactive"));

        let mut fake = Fake::default();
        fake.selection = 0;
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "no_selection"));

        let mut fake = Fake::default();
        fake.bad.push(6);
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "bad_selection"));

        let mut fake = Fake::default();
        fake.scope = 1;
        assert_eq!(
            doc_editable_with(&fake, Doc(4)),
            (false, "edit_scope_active")
        );

        let mut fake = Fake::default();
        fake.undo = 0;
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "no_history"));

        let mut fake = Fake::default();
        fake.bad.push(7);
        assert_eq!(doc_editable_with(&fake, Doc(4)), (false, "bad_history"));

        let mut fake = Fake::default();
        fake.active_hist = 8;
        assert_eq!(
            doc_editable_with(&fake, Doc(4)),
            (false, "history_not_active")
        );
    }

    // -----------------------------------------------------------------------------------------
    // The fingerprint (`mcp_bridge.c:1746-1775`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn fingerprint_bumps_the_epoch_only_on_a_change_and_the_doc_id_only_on_a_new_session() {
        let mut fp = DocFp::default();
        let first = FpSample {
            doc_session: 0x1000,
            root: 0x2000,
            path_ptr: 0x3000,
            root_handle: 7,
        };
        assert!(fingerprint_step(&mut fp, first));
        assert_eq!((fp.doc_seq, fp.epoch), (1, 1));

        assert!(!fingerprint_step(&mut fp, first));
        assert_eq!((fp.doc_seq, fp.epoch), (1, 1));

        // A new root at the same session: epoch only (`:1767-1771`).
        let moved = FpSample {
            root: 0x2008,
            ..first
        };
        assert!(fingerprint_step(&mut fp, moved));
        assert_eq!((fp.doc_seq, fp.epoch), (1, 2));

        // A close/open that reuses every pointer but the path: the document id moves
        // (`:1765-1766`). The `doc_session` too here.
        let reopened = FpSample {
            doc_session: 0x1008,
            ..moved
        };
        assert!(fingerprint_step(&mut fp, reopened));
        assert_eq!((fp.doc_seq, fp.epoch), (2, 3));

        // Only the world handle changed: epoch.
        let rehandled = FpSample {
            root_handle: 9,
            ..reopened
        };
        assert!(fingerprint_step(&mut fp, rehandled));
        assert_eq!((fp.doc_seq, fp.epoch), (2, 4));
    }

    // -----------------------------------------------------------------------------------------
    // `history_top_info` (`mcp_bridge.c:1810-1824`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn history_top_info_copies_the_top_group_exactly() {
        assert_eq!(
            history_top_info(0, None, 128),
            HistoryInfo {
                name: Vec::new(),
                depth: 0,
                serial: 0
            }
        );
        // No top entry: depth survives, serial/name stay 0/("").
        assert_eq!(
            history_top_info(4, None, 128),
            HistoryInfo {
                name: Vec::new(),
                depth: 4,
                serial: 0
            }
        );
        let mut name = [0u8; 128];
        name[..7].copy_from_slice(b"MCP: mv");
        assert_eq!(
            history_top_info(3, Some(UndoGroupView { serial: 11, name: &name }), 128),
            HistoryInfo {
                name: b"MCP: mv".to_vec(),
                depth: 3,
                serial: 11
            }
        );
        // `i + 1 < namesz`: with namesz 4 at most 3 bytes, even without a NUL.
        let full = [b'x'; 128];
        assert_eq!(
            history_top_info(1, Some(UndoGroupView { serial: 1, name: &full }), 4),
            HistoryInfo {
                name: b"xxx".to_vec(),
                depth: 1,
                serial: 1
            }
        );
        // The field-size bound: `i < sizeof entry->m_szName` (128) caps the copy at 128 bytes
        // even when the caller's output buffer is larger (`:1820`).
        assert_eq!(
            history_top_info(1, Some(UndoGroupView { serial: 2, name: &full }), 4096).name.len(),
            128
        );
    }

    // -----------------------------------------------------------------------------------------
    // `doc.active`'s two reply shapes (`mcp_bridge.c:6234-6277`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn doc_active_null_reply_is_the_c_bytes() {
        // `:6243-6248`: envelope, `"result":{"doc":null,"reason":<reason>}`, close + newline.
        assert_eq!(
            doc_active_reply("q1", None, "no_session"),
            r#"{"jsonrpc":"2.0","id":"q1","bridge":{"protocol":1,"build":"m3.0"},"result":{"doc":null,"reason":"no_session"}}"#
                .replace("m3.0", crate::BRIDGE_BUILD)
                + "\n"
        );
    }

    #[test]
    fn doc_active_full_reply_is_the_c_bytes() {
        let facts = DocFacts {
            path: br"C:\maps\a.vmap".to_vec(),
            dirty: Some(true),
            active: true,
            editable: false,
            why: "no_selection",
            is_new: true,
            undo: HistoryInfo {
                name: b"MCP: move".to_vec(),
                depth: 2,
                serial: 11,
            },
        };
        // `:6250-6272`, in the C's member order.
        assert_eq!(
            doc_active_reply("q2", Some((3, 9, &facts)), "ok"),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"q2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{"
            ) + r#""doc":{"id":"d3","path":"C:\\maps\\a.vmap","dirty":true,"active":true,"editable":false,"notEditableReason":"no_selection","epoch":9,"docType":"vmap","isNewFile":true,"undoName":"MCP: move","undoDepth":2,"topSerial":11}"#
                + "}}\n"
        );

        // A clean, editable document with no undo group: `dirty:null`, `undoName:null`.
        let facts = DocFacts {
            path: Vec::new(),
            dirty: None,
            active: false,
            editable: true,
            why: "ok",
            is_new: false,
            undo: HistoryInfo::default(),
        };
        assert_eq!(
            doc_active_reply("q3", Some((0, 0, &facts)), "ok"),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"q3\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{"
            ) + r#""doc":{"id":"d0","path":"","dirty":null,"active":false,"editable":true,"notEditableReason":"ok","epoch":0,"docType":"vmap","isNewFile":false,"undoName":null,"undoDepth":0,"topSerial":0}"#
                + "}}\n"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `entity.get`'s result builder (`mcp_bridge.c:3715-3838`)
    // -----------------------------------------------------------------------------------------

    fn build_entity_get(id: i64, read: &EntityRead) -> String {
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        assert!(
            !entity_get_into(&mut out, id, read),
            "both name lists fit their 1024-byte builders"
        );
        String::from_utf8_lossy(out.as_cstr()).into_owned()
    }

    #[test]
    fn entity_get_builder_is_the_c_byte_sequence() {
        let read = EntityRead {
            classname: b"a".to_vec(),
            targetname: Vec::new(),
            origin: [1.0, 2.0, 3.0],
            angles: None,
            scales: None,
            attrs: vec![
                (b"health".to_vec(), AttrValue::Int(10)),
                (b"model".to_vec(), AttrValue::Str(b"m".to_vec())),
            ],
            truncated: false,
            connections: ConnList::Null,
            children: ChildList::Null,
        };
        // `:3725-3840`: nodeID, classname, targetname, origin, angles, scales, keyValues,
        // keyValueCount, unsupported, truncatedValues, connections, children, source.
        assert_eq!(
            build_entity_get(7, &read),
            r#"{"nodeID":7,"classname":"a","targetname":"","origin":[1,2,3],"angles":null,"scales":null,"keyValues":{"health":10,"model":"m"},"keyValueCount":2,"unsupported":[],"truncatedValues":[],"connections":null,"children":null,"source":"live"}"#
        );

        let read = EntityRead {
            classname: b"x".to_vec(),
            targetname: b"t\"q".to_vec(),
            origin: [-1.5, 0.0, 2.25],
            angles: Some([10.0, -20.0, 30.5]),
            scales: Some([1.0, 1.0, 1.0]),
            attrs: vec![
                (
                    b"color".to_vec(),
                    AttrValue::Null {
                        unsupported: true,
                        cut: false,
                    },
                ),
                (
                    b"custom".to_vec(),
                    AttrValue::Null {
                        unsupported: false,
                        cut: true,
                    },
                ),
                (b"spawnflags".to_vec(), AttrValue::Bool(true)),
                (b"speed".to_vec(), AttrValue::Float(1.5)),
            ],
            truncated: true,
            connections: ConnList::List {
                entries: vec![Conn {
                    output: b"OnTrigger".to_vec(),
                    target_type: 7,
                    target: b"t".to_vec(),
                    input: b"Use".to_vec(),
                    param: Vec::new(),
                    delay: 0.5,
                    times_to_fire: 1,
                    cut: false,
                }],
                total: Some(513),
            },
            children: ChildList::List {
                entries: vec![
                    ChildEntry {
                        id: Some(9),
                        class: "mesh",
                    },
                    ChildEntry {
                        id: None,
                        class: "other",
                    },
                    // `:3682`: a negative id is `null`, never a node a reader could address.
                    ChildEntry {
                        id: Some(-3),
                        class: "entity",
                    },
                ],
                total: None,
            },
        };
        assert_eq!(
            build_entity_get(8, &read),
            r#"{"nodeID":8,"classname":"x","targetname":"t\"q","origin":[-1.500,0,2.250],"angles":[10,-20,30.500],"scales":[1,1,1],"keyValues":{"color":null,"custom":null,"spawnflags":true,"speed":1.500},"keyValueCount":4,"unsupported":["color"],"truncatedValues":["custom"],"truncated":true,"connections":[{"outputName":"OnTrigger","targetType":7,"targetName":"t","inputName":"Use","overrideParam":"","delay":0.500,"timesToFire":1}],"connectionsTruncated":true,"connectionsTotal":513,"children":[{"nodeID":9,"class":"mesh"},{"nodeID":null,"class":"other"},{"nodeID":null,"class":"entity"}],"source":"live"}"#
        );
    }

    #[test]
    fn a_negative_child_node_id_prints_null_like_the_c() {
        // `:3682`: `if (id >= 0) sb_addi(out, id); else sb_add(out, "null")` - `node_id_of`
        // leaves `id` at -1 when the nodeID attribute is missing or of the wrong type, and the
        // reply must never name a node a reader would then address.
        let list = ChildList::List {
            entries: vec![
                ChildEntry {
                    id: Some(-1),
                    class: "mesh",
                },
                ChildEntry {
                    id: Some(0),
                    class: "entity",
                },
            ],
            total: None,
        };
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        assert_eq!(children_into(&mut out, &list), None);
        assert_eq!(
            String::from_utf8_lossy(out.as_cstr()),
            r#"[{"nodeID":null,"class":"mesh"},{"nodeID":0,"class":"entity"}]"#
        );
    }

    #[test]
    fn entity_get_name_lists_overflow_their_1024_byte_builders() {
        // `:3752-3755`: `static char ubuf[1024], vbuf[1024]`; `:3834` `if (us.ovf || vs.ovf)
        // out->ovf = 1`. Sixty-four names, each escaped past 16 bytes, cannot fit `ubuf`.
        let attrs: Vec<(Vec<u8>, AttrValue)> = (0..64)
            .map(|i| {
                (
                    format!("unsupported_attribute_{i:03}").into_bytes(),
                    AttrValue::Null {
                        unsupported: true,
                        cut: false,
                    },
                )
            })
            .collect();
        let read = EntityRead {
            classname: Vec::new(),
            targetname: Vec::new(),
            origin: [0.0, 0.0, 0.0],
            angles: None,
            scales: None,
            attrs,
            truncated: false,
            connections: ConnList::Null,
            children: ChildList::Null,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut out = Sb::new(&mut buf);
        assert!(entity_get_into(&mut out, 1, &read), "ubuf must overflow");

        // The op answers the C's `-32603 entity has too many or too large keyvalues`
        // (`geo_dispatch:5774-5775`) before any cut JSON reaches the wire.
        let fake = tree().with_node(
            21,
            FakeNode {
                class: "entity",
                id: Some(6),
                origin: Some([0.0, 0.0, 0.0]),
                entity: Some(read),
                ..FakeNode::default()
            },
        );
        assert_eq!(
            run_entity_get(&fake, Some(r#"{"nodeID":6}"#)),
            Err(OpErr::bare(
                -32603,
                "entity has too many or too large keyvalues"
            ))
        );
    }

    #[test]
    fn invalid_utf8_game_text_becomes_one_replacement_character_as_the_c() {
        // ED A0 80 is a surrogate encoded as UTF-8: `utf8_seq` rejects it (`mcp_bridge.c:278`)
        // and `sb_addjson` emits one U+FFFD per maximal subpart (`:311-318`), not one per byte.
        let read = EntityRead {
            classname: Vec::new(),
            targetname: vec![0xED, 0xA0, 0x80],
            origin: [0.0, 0.0, 0.0],
            angles: None,
            scales: None,
            attrs: vec![(b"label".to_vec(), AttrValue::Str(vec![0xED, 0xA0, 0x80]))],
            truncated: false,
            connections: ConnList::Null,
            children: ChildList::Null,
        };
        let json = build_entity_get(1, &read);
        assert!(json.contains("\"targetname\":\"\u{FFFD}\""), "{json}");
        assert!(json.contains("\"label\":\"\u{FFFD}\""), "{json}");
        assert!(!json.contains("\u{FFFD}\u{FFFD}"), "{json}");
    }

    // -----------------------------------------------------------------------------------------
    // `node.list` (`mcp_bridge.c:5512-5538`) and `entity.find` (`:4638-4668`)
    // -----------------------------------------------------------------------------------------

    fn tree() -> Fake {
        Fake::default()
            .with_node(
                10,
                FakeNode {
                    class: "world",
                    children: Some(vec![20, 21, 22]),
                    ..FakeNode::default()
                },
            )
            .with_node(
                20,
                FakeNode {
                    class: "mesh",
                    id: Some(5),
                    origin: Some([1.0, 2.0, 3.0]),
                    mesh: Some((8, 2)),
                    ..FakeNode::default()
                },
            )
            .with_node(
                21,
                FakeNode {
                    class: "entity",
                    id: Some(6),
                    origin: Some([0.0, 0.0, 0.0]),
                    strings: Some((b"func_door".to_vec(), b"d1".to_vec())),
                    ..FakeNode::default()
                },
            )
            .with_node(
                22,
                FakeNode {
                    class: "entity",
                    id: Some(7),
                    origin: Some([10.0, 10.0, 10.0]),
                    strings: Some((b"func_button".to_vec(), b"b".to_vec())),
                    ..FakeNode::default()
                },
            )
    }

    #[test]
    fn node_list_walk_is_the_c_byte_sequence() {
        // `:5519-5533`: nodes[], count, visited, truncated, source. The mesh branch adds the
        // two component counts (`:3207-3208`).
        assert_eq!(
            run_node_list(&tree(), "{}"),
            r#"{"nodes":[{"nodeID":5,"class":"mesh","origin":[1,2,3],"vertexSlots":8,"faceSlots":2},{"nodeID":6,"class":"entity","origin":[0,0,0]},{"nodeID":7,"class":"entity","origin":[10,10,10]}],"count":3,"visited":3,"truncated":false,"source":"live"}"#
        );
        // A class filter still visits every node (`:3168-3200`): `visited` is the walk's count.
        assert_eq!(
            run_node_list(&tree(), r#"{"class":"entity"}"#),
            r#"{"nodes":[{"nodeID":6,"class":"entity","origin":[0,0,0]},{"nodeID":7,"class":"entity","origin":[10,10,10]}],"count":2,"visited":3,"truncated":false,"source":"live"}"#
        );
        assert_eq!(
            run_node_list(&tree(), r#"{"class":"all"}"#),
            run_node_list(&tree(), "{}")
        );
        // `limit` clamps silently to 2000 outside 1..10000 (`:5516`).
        assert_eq!(
            run_node_list(&tree(), r#"{"limit":1}"#),
            r#"{"nodes":[{"nodeID":5,"class":"mesh","origin":[1,2,3],"vertexSlots":8,"faceSlots":2}],"count":1,"visited":3,"truncated":true,"source":"live"}"#
        );
    }

    #[test]
    fn node_list_without_a_world_is_an_internal_error() {
        let mut fake = tree();
        fake.world = None;
        assert_eq!(
            fake.reason(Some("{}")),
            Err(OpErr::new(-32603, "InternalError", "no world node"))
        );
    }

    #[test]
    fn entity_find_filters_case_insensitively_and_by_radius() {
        // `:4644-4656`: both filters are ASCII-case-insensitive; `:4596-4600`: within.
        assert_eq!(
            run_entity_find(&tree(), r#"{"class":"FUNC_DOOR"}"#),
            r#"{"entities":[{"nodeID":6,"classname":"func_door","targetname":"d1","origin":[0,0,0]}],"count":1,"visited":3,"truncated":false,"source":"live"}"#
        );
        assert_eq!(
            run_entity_find(&tree(), r#"{"targetname":"B"}"#),
            r#"{"entities":[{"nodeID":7,"classname":"func_button","targetname":"b","origin":[10,10,10]}],"count":1,"visited":3,"truncated":false,"source":"live"}"#
        );
        assert_eq!(
            run_entity_find(&tree(), r#"{"within":{"origin":[0,0,0],"radius":5}}"#),
            r#"{"entities":[{"nodeID":6,"classname":"func_door","targetname":"d1","origin":[0,0,0]}],"count":1,"visited":3,"truncated":false,"source":"live"}"#
        );
        // The radius is exclusive: 10,10,10 is sqrt(300) away from 0,0,0.
        assert_eq!(
            run_entity_find(&tree(), r#"{"within":{"origin":[0,0,0],"radius":10}}"#),
            r#"{"entities":[{"nodeID":6,"classname":"func_door","targetname":"d1","origin":[0,0,0]}],"count":1,"visited":3,"truncated":false,"source":"live"}"#
        );
        // limit: 1 -> truncated, the walk still visits everything (`:4598-4600`).
        assert_eq!(
            run_entity_find(&tree(), r#"{"limit":1}"#),
            r#"{"entities":[{"nodeID":6,"classname":"func_door","targetname":"d1","origin":[0,0,0]}],"count":1,"visited":3,"truncated":true,"source":"live"}"#
        );
    }

    #[test]
    fn entity_find_param_errors_are_the_c_codes_and_messages() {
        let fake = tree();
        assert_eq!(
            op_entity_find(&fake, Doc(4), Some(r#"{"class":""}"#), &mut Sb::new(&mut [0u8; 64])),
            Err(OpErr::new(
                -32602,
                "InvalidParams",
                "class: a non-empty string is required"
            ))
        );
        assert_eq!(
            op_entity_find(&fake, Doc(4), Some(r#"{"targetname":""}"#), &mut Sb::new(&mut [0u8; 64])),
            Err(OpErr::new(
                -32602,
                "InvalidParams",
                "targetname: a non-empty string is required"
            ))
        );
        for limit in ["0", "2001", "-3", "1.5", "\"x\""] {
            let params = format!(r#"{{"limit":{limit}}}"#);
            assert_eq!(
                op_entity_find(&fake, Doc(4), Some(&params), &mut Sb::new(&mut [0u8; 64])),
                Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "limit: an int in 1..2000 is required"
                )),
                "limit {limit}"
            );
        }
        for within in [
            r#"{"within":5}"#,
            r#"{"within":{}}"#,
            r#"{"within":{"origin":[0,0,0]}}"#,
            r#"{"within":{"origin":[0,0],"radius":1}}"#,
            r#"{"within":{"origin":[0,0,0],"radius":0}}"#,
            r#"{"within":{"origin":[0,0,0],"radius":-1}}"#,
        ] {
            assert_eq!(
                op_entity_find(&fake, Doc(4), Some(within), &mut Sb::new(&mut [0u8; 64])),
                Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "within: {origin:[x,y,z], radius > 0} required"
                )),
                "{within}"
            );
        }
        let mut no_world = tree();
        no_world.world = None;
        assert_eq!(
            op_entity_find(&no_world, Doc(4), None, &mut Sb::new(&mut [0u8; 64])),
            Err(OpErr::new(-32603, "InternalError", "no world node"))
        );
    }

    #[test]
    fn entity_get_param_errors_are_the_c_codes_and_messages() {
        let fake = tree();
        let missing = OpErr::new(-32602, "InvalidParams", "nodeID required");
        assert_eq!(run_entity_get(&fake, None), Err(missing.clone()));
        assert_eq!(run_entity_get(&fake, Some("{}")), Err(missing));
        let bounds = OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        );
        for params in [
            r#"{"nodeID":"x"}"#,
            r#"{"nodeID":-1}"#,
            r#"{"nodeID":1.5}"#,
            r#"{"nodeID":2147483648}"#,
            r#"{"nodeID":1e3}"#,
        ] {
            assert_eq!(run_entity_get(&fake, Some(params)), Err(bounds.clone()), "{params}");
        }
        assert_eq!(
            run_entity_get(&fake, Some(r#"{"nodeID":99}"#)),
            Err(OpErr::new(
                -32022,
                "NodeNotFound",
                "no node with this nodeID in the live document"
            ))
        );
        // node 5 is the mesh: not an entity (`:3450-3453`).
        assert_eq!(
            run_entity_get(&fake, Some(r#"{"nodeID":5}"#)),
            Err(OpErr::new(-32602, "InvalidParams", "not an entity"))
        );
        // The entity has no properties element: `-32603` (`:3701-3703`).
        assert_eq!(
            run_entity_get(&fake, Some(r#"{"nodeID":6}"#)),
            Err(OpErr::new(-32603, "InternalError", "entity_properties missing"))
        );
    }

    #[test]
    fn entity_get_reaches_the_builder_for_an_entity() {
        let read = EntityRead {
            classname: b"func_door".to_vec(),
            targetname: b"d1".to_vec(),
            origin: [0.0, 0.0, 0.0],
            angles: None,
            scales: Some([1.0, 1.0, 1.0]),
            attrs: vec![],
            truncated: false,
            connections: ConnList::Null,
            children: ChildList::Null,
        };
        let fake = tree().with_node(
            21,
            FakeNode {
                class: "entity",
                id: Some(6),
                origin: Some([0.0, 0.0, 0.0]),
                entity: Some(read),
                ..FakeNode::default()
            },
        );
        assert_eq!(
            run_entity_get(&fake, Some(r#"{"nodeID":6}"#)),
            Ok(r#"{"nodeID":6,"classname":"func_door","targetname":"d1","origin":[0,0,0],"angles":null,"scales":[1,1,1],"keyValues":{},"keyValueCount":0,"unsupported":[],"truncatedValues":[],"connections":null,"children":null,"source":"live"}"#.to_string())
        );
    }

    // -----------------------------------------------------------------------------------------
    // Parameter validation precedes every node read (the brief's rule): the fake's `children`
    // would answer `None` for a missing world, and the errors above came back instead.
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_document_gate_precedes_validation_as_in_geo_dispatch() {
        let mut fake = tree();
        fake.session = None;
        let mut buf = vec![0u8; 64];
        let mut out = Sb::new(&mut buf);
        assert_eq!(
            geo_read(&fake, Some("{}"), &mut out, |hammer, doc, params, out| {
                op_entity_get(hammer, doc, params, out)
            }),
            Err(OpErr::new(-32003, "NoActiveDocument", "no_session"))
        );
    }

    // -----------------------------------------------------------------------------------------
    // The handler table
    // -----------------------------------------------------------------------------------------

    #[test]
    fn every_handler_names_a_c_row_this_group_implements() {
        let names: Vec<&str> = HANDLERS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "entity.create",
                "undo.undo",
                "undo.redo",
                "entity.setKeyValues",
                "node.delete",
                "node.list",
                "node.setTransform",
                "entity.addConnections",
                "entity.removeConnections",
                "entity.get",
                "entity.find",
                "doc.active",
            ]
        );
        for name in names {
            assert!(
                crate::registry::find(name).is_some_and(|row| row.implemented),
                "{name} is not an implemented C row"
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // The write fake: a scripted, recording `Write` over `Fake`'s fabricated document. Every
    // call records its arguments so a test can pin the C's call order (and prove that a refused
    // request never reaches the game).
    // -----------------------------------------------------------------------------------------

    /// One attribute's scripted facts (`attr_info` + `attr_previous` + `kv_case_variant`).
    #[derive(Clone, Debug)]
    struct AttrScript {
        present: bool,
        readable: bool,
        kind: i32,
        previous: PrevValue,
        variant: Option<Vec<u8>>,
    }

    impl Default for AttrScript {
        fn default() -> Self {
            Self {
                present: true,
                readable: true,
                kind: AT_AT_STRING as i32,
                previous: PrevValue::Null,
                variant: None,
            }
        }
    }

    /// The write side's script and call log.
    struct WScript {
        calls: Vec<String>,
        class_exists: bool,
        create: Option<usize>,
        /// `create_node` writes these into `target.origin`/`target.angles` before it returns -
        /// `CreateNodeByName`'s `float *` parameters (`s2sdk.h:704`), which the C prints
        /// after the call (`mcp_bridge.c:2122-2129`).
        writeback: Option<([f32; 3], [f32; 3])>,
        properties: Option<usize>,
        attrs: BTreeMap<Vec<u8>, AttrScript>,
        conns: Vec<Conn>,
        add_ok: bool,
        remove_ok: bool,
        locked: bool,
        angles: Option<[f32; 3]>,
        scales: Option<[f32; 3]>,
        redo: usize,
        history_ok: bool,
        history: Vec<HistoryInfo>,
    }

    impl Default for WScript {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                class_exists: true,
                create: None,
                writeback: None,
                properties: Some(0x2000),
                attrs: BTreeMap::new(),
                conns: Vec::new(),
                add_ok: true,
                remove_ok: true,
                locked: false,
                angles: Some([0.0, 0.0, 0.0]),
                scales: Some([1.0, 1.0, 1.0]),
                redo: 8,
                history_ok: true,
                history: vec![HistoryInfo::default()],
            }
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn vec3(value: [f32; 3]) -> String {
        format!("[{},{},{}]", value[0], value[1], value[2])
    }

    /// A one-key `KvApply` with the key applied and no warning.
    fn applied(name: &[u8]) -> KvApply {
        KvApply {
            entries: vec![KvEntry {
                name: name.to_vec(),
                applied: true,
                warning: None,
            }],
            previous: None,
            stalled: false,
        }
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.script.borrow().calls.clone()
        }
        fn record(&self, call: String) {
            self.script.borrow_mut().calls.push(call);
        }
        fn script<T>(&self, f: impl FnOnce(&WScript) -> T) -> T {
            f(&self.script.borrow())
        }
        fn script_mut<T>(&self, f: impl FnOnce(&mut WScript) -> T) -> T {
            f(&mut self.script.borrow_mut())
        }
    }

    impl Write for Fake {
        fn class_exists(&self, cls: &[u8]) -> bool {
            self.record(format!("class_exists {}", text(cls)));
            self.script(|s| s.class_exists)
        }
        fn create_node(&self, doc: Doc, target: &mut CreateTarget) -> Option<Node> {
            self.record(format!(
                "create doc={} class={} origin={} angles={} scales={}",
                doc.0,
                text(&target.cls),
                vec3(target.origin),
                vec3(target.angles),
                vec3(target.scales)
            ));
            if let Some((origin, angles)) = self.script(|s| s.writeback) {
                target.origin = origin;
                target.angles = angles;
            }
            self.script(|s| s.create).map(Node)
        }
        fn properties(&self, _node: Node) -> Option<usize> {
            self.record("properties".to_string());
            self.script(|s| s.properties)
        }
        fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize {
            self.record(format!(
                "undo_open doc={} label={}",
                doc.0,
                text(label.to_bytes())
            ));
            7
        }
        fn record_node_tree(&self, hist: usize, node: Node) {
            self.record(format!("record hist={hist} node={}", node.0));
        }
        fn doc_set_modified(&self, doc: Doc) {
            self.record(format!("modified doc={}", doc.0));
        }
        fn get_active_history(&self) -> usize {
            self.record("active_history".to_string());
            7
        }
        fn get_redo_history(&self, _doc: Doc) -> usize {
            self.record("redo_history".to_string());
            self.script(|s| s.redo)
        }
        fn history_ok(&self, hist: usize) -> bool {
            self.record(format!("history_ok {hist}"));
            self.script(|s| s.history_ok)
        }
        fn history_info(&self, _hist: usize) -> HistoryInfo {
            self.record("history_info".to_string());
            let mut s = self.script.borrow_mut();
            if s.history.len() > 1 {
                s.history.remove(0)
            } else {
                s.history.first().cloned().unwrap_or_default()
            }
        }
        fn undo_pop(&self, hist: usize) {
            self.record(format!("undo_pop {hist}"));
        }
        fn node_locked(&self, _node: Node) -> bool {
            self.record("node_locked".to_string());
            self.script(|s| s.locked)
        }
        fn delete_node(&self, doc: Doc, node: Node) {
            self.record(format!("delete doc={} node={}", doc.0, node.0));
        }
        fn set_origin(&self, _node: Node, value: [f32; 3]) {
            self.record(format!("set_origin {}", vec3(value)));
        }
        fn set_angles(&self, _node: Node, value: [f32; 3]) {
            self.record(format!("set_angles {}", vec3(value)));
        }
        fn set_scales(&self, _node: Node, value: [f32; 3]) {
            self.record(format!("set_scales {}", vec3(value)));
        }
        fn angles(&self, _node: Node) -> Option<[f32; 3]> {
            self.record("angles".to_string());
            self.script(|s| s.angles)
        }
        fn scales(&self, _node: Node) -> Option<[f32; 3]> {
            self.record("scales".to_string());
            self.script(|s| s.scales)
        }
        fn attr_previous(&self, _element: usize, name: &[u8]) -> PrevValue {
            self.record(format!("previous {}", text(name)));
            self.script(|s| {
                s.attrs
                    .get(name)
                    .map_or(PrevValue::Null, |attr| attr.previous.clone())
            })
        }
        fn attr_info(&self, _element: usize, name: &[u8]) -> AttrInfo {
            self.record(format!("attr_info {}", text(name)));
            self.script(|s| match s.attrs.get(name) {
                Some(attr) => AttrInfo {
                    symbol: true,
                    attr: if attr.present { 0x1000 } else { 0 },
                    present: attr.present,
                    readable: attr.readable,
                    kind: attr.kind,
                },
                // The data model interned the name and the element does not hold it: the C's
                // creatable key (`sym.m_pString` set, `FindAttribute` null).
                None => AttrInfo {
                    symbol: true,
                    attr: 0,
                    present: false,
                    readable: false,
                    kind: -1,
                },
            })
        }
        fn case_variant(&self, _element: usize, name: &[u8]) -> Option<Vec<u8>> {
            self.record(format!("variant {}", text(name)));
            self.script(|s| s.attrs.get(name).and_then(|attr| attr.variant.clone()))
        }
        fn set_key_value(&self, _node: Node, name: &[u8], value: &[u8]) {
            self.record(format!("set_kv {}={}", text(name), text(value)));
        }
        fn set_value_int(&self, _attr: usize, value: i32) {
            self.record(format!("set_int {value}"));
        }
        fn set_value_float(&self, _attr: usize, value: f32) {
            self.record(format!("set_float {value}"));
        }
        fn conn_count(&self, _node: Node) -> Option<usize> {
            self.record("conn_count".to_string());
            self.script(|s| Some(s.conns.len()))
        }
        fn conn_entry(&self, _node: Node, index: usize) -> Option<Conn> {
            self.record(format!("conn_entry {index}"));
            self.script(|s| s.conns.get(index).cloned())
        }
        fn conn_filter_fields(&self, _node: Node, index: usize) -> Option<FilterFields> {
            self.record(format!("conn_filter {index}"));
            self.script(|s| {
                s.conns.get(index).map(|conn| FilterFields {
                    output: (!conn.cut).then(|| conn.output.clone()),
                    target: (!conn.cut).then(|| conn.target.clone()),
                    input: (!conn.cut).then(|| conn.input.clone()),
                })
            })
        }
        fn add_connection(&self, _node: Node, spec: &ConnSpec) -> (bool, Option<i32>) {
            self.record(format!(
                "add output={} target={} input={} param={} delay={} times={}",
                text(spec.output()),
                text(spec.target()),
                text(spec.input()),
                text(spec.param()),
                spec.delay,
                spec.times_to_fire
            ));
            let mut s = self.script.borrow_mut();
            if !s.add_ok {
                return (false, None);
            }
            let index = s.conns.len() as i32;
            s.conns.push(Conn {
                output: spec.output().to_vec(),
                target_type: 7,
                target: spec.target().to_vec(),
                input: spec.input().to_vec(),
                param: spec.param().to_vec(),
                delay: spec.delay,
                times_to_fire: spec.times_to_fire,
                cut: false,
            });
            (true, Some(index))
        }
        fn remove_connection(&self, _node: Node, index: i32) -> bool {
            self.record(format!("remove {index}"));
            let mut s = self.script.borrow_mut();
            if !s.remove_ok {
                return false;
            }
            if (index as usize) < s.conns.len() {
                s.conns.remove(index as usize);
            }
            true
        }
    }

    fn run_write(
        fake: &Fake,
        f: impl FnOnce(&dyn Write, Doc, Option<&str>, &mut Sb) -> Result<bool, OpErr>,
        params: Option<&str>,
    ) -> (Result<bool, OpErr>, String) {
        let mut buf = vec![0u8; 65536];
        let mut out = Sb::new(&mut buf);
        let outcome = f(fake, Doc(4), params, &mut out);
        (outcome, String::from_utf8_lossy(out.as_cstr()).into_owned())
    }


    // -----------------------------------------------------------------------------------------
    // `expect_doc_ok` (`mcp_bridge.c:1928-1948`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn expect_doc_ok_reports_each_refusal_in_the_c_order() {
        let ok = br#"{"expectDoc":{"id":"d1","epoch":9}}"#;
        assert_eq!(expect_doc_ok(Some(ok), 1, 9), Ok(()));
        // Every digit of the id counts, wherever it sits (`:1939-1940`).
        assert_eq!(
            expect_doc_ok(Some(br#"{"expectDoc":{"id":"x1y","epoch":9}}"#), 1, 9),
            Ok(())
        );
        for (params, why) in [
            (None, "missing"),
            (Some(&b"{}"[..]), "missing"),
            (Some(&br#"{"expectDoc":5}"#[..]), "missing"),
            (Some(&br#"{"expectDoc":{"epoch":9}}"#[..]), "missing_id"),
            (Some(&br#"{"expectDoc":{"id":5,"epoch":9}}"#[..]), "missing_id"),
            (Some(&br#"{"expectDoc":{"id":"d1"}}"#[..]), "missing_epoch"),
            (Some(&br#"{"expectDoc":{"id":"d2","epoch":9}}"#[..]), "doc_id"),
            (Some(&br#"{"expectDoc":{"id":"d1","epoch":8}}"#[..]), "epoch"),
        ] {
            assert_eq!(expect_doc_ok(params, 1, 9), Err(why), "{params:?}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // The handle table (`mcp_bridge.c:1893-1926`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn a_handle_is_n_then_a_decimal_and_a_stale_token_is_named() {
        let token = handle_new(0x1234);
        assert!(token.starts_with('n'), "{token}");
        assert!(
            token[1..].bytes().all(|byte| byte.is_ascii_digit()),
            "{token}"
        );
        assert_eq!(handle_lookup(token.as_bytes()), Ok(Node(0x1234)));
        assert_eq!(handle_lookup(b"n999999999"), Err("unknown_handle"));
        assert_eq!(handle_lookup(b""), Err("unknown_handle"));
    }

    // -----------------------------------------------------------------------------------------
    // `entity.create`'s target and reply (`mcp_bridge.c:2063-2157`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn entity_create_target_validation_is_the_c_codes_and_text() {
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "class")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "class")),
            (
                Some(r#"{"class":""}"#),
                OpErr::new(-32602, "InvalidParams", "class"),
            ),
            (
                Some(r#"{"class":5}"#),
                OpErr::new(-32602, "InvalidParams", "class"),
            ),
            (
                Some(r#"{"class":"x","origin":[1,2]}"#),
                OpErr::new(-32602, "InvalidParams", "origin: [x,y,z] required"),
            ),
            (
                Some(r#"{"class":"x","angles":"y"}"#),
                OpErr::new(-32602, "InvalidParams", "angles: [pitch,yaw,roll] required"),
            ),
            (
                Some(r#"{"class":"x","scales":5}"#),
                OpErr::new(-32602, "InvalidParams", "scales: [x,y,z] required"),
            ),
            (
                Some(r#"{"class":"x","scales":[0,1,1]}"#),
                OpErr::new(-32602, "InvalidParams", "scales: each finite and in (0, 1000]"),
            ),
            (
                Some(r#"{"class":"x","scales":[1,1,1001]}"#),
                OpErr::new(-32602, "InvalidParams", "scales: each finite and in (0, 1000]"),
            ),
        ];
        for (params, expected) in cases {
            assert_eq!(create_target_parse(*params), Err(expected.clone()), "{params:?}");
        }
        assert_eq!(
            create_target_parse(Some(r#"{"class":"func_door"}"#)),
            Ok(CreateTarget {
                cls: b"func_door".to_vec(),
                origin: [0.0, 0.0, 0.0],
                angles: [0.0, 0.0, 0.0],
                scales: [1.0, 1.0, 1.0],
                has_scales: false,
            })
        );
        assert_eq!(
            create_target_parse(Some(
                r#"{"class":"prop_dynamic","origin":[1,2,3],"angles":[4,5,6],"scales":[2,2,2]}"#
            )),
            Ok(CreateTarget {
                cls: b"prop_dynamic".to_vec(),
                origin: [1.0, 2.0, 3.0],
                angles: [4.0, 5.0, 6.0],
                scales: [2.0, 2.0, 2.0],
                has_scales: true,
            })
        );
    }

    #[test]
    fn entity_create_reply_is_the_c_byte_sequence() {
        let target = CreateTarget {
            cls: b"func_door".to_vec(),
            origin: [1.0, 2.0, 3.0],
            angles: [0.0, 0.0, 0.0],
            scales: [1.0, 1.0, 1.0],
            has_scales: false,
        };
        let mut buf = vec![0u8; 512];
        let mut out = Sb::new(&mut buf);
        assert!(!create_reply_into(&mut out, b"n7", Some(6), &target, None));
        assert_eq!(
            out.as_cstr(),
            br#"{"handle":"n7","nodeID":6,"class":"func_door","origin":[1,2,3],"scales":[1,1,1],"undoLabel":"MCP: create entity"}"#
        );

        // No readable nodeID: the member is absent (`:2138-2141`).
        let mut buf = vec![0u8; 512];
        let mut out = Sb::new(&mut buf);
        assert!(!create_reply_into(&mut out, b"n7", None, &target, None));
        assert_eq!(
            out.as_cstr(),
            br#"{"handle":"n7","class":"func_door","origin":[1,2,3],"scales":[1,1,1],"undoLabel":"MCP: create entity"}"#
        );

        // The keyValues echo comes last, inside the object (`:2152-2155`).
        let mut buf = vec![0u8; 512];
        let mut out = Sb::new(&mut buf);
        assert!(!create_reply_into(
            &mut out,
            b"n7",
            Some(6),
            &target,
            Some(&applied(b"health"))
        ));
        assert_eq!(
            out.as_cstr(),
            br#"{"handle":"n7","nodeID":6,"class":"func_door","origin":[1,2,3],"scales":[1,1,1],"undoLabel":"MCP: create entity","keyValues":{"applied":{"health":true},"warnings":[]}}"#
        );
    }

    #[test]
    fn entity_create_skips_the_keyvalues_echo_without_a_properties_element() {
        // `:2151-2155`: the echo is written only when `GetEntityProperties` answered a node.
        let fake = Fake::default().with_node(
            30,
            FakeNode {
                class: "entity",
                id: Some(6),
                ..FakeNode::default()
            },
        );
        fake.script_mut(|s| {
            s.create = Some(30);
            s.properties = None;
        });
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create(w, doc, params, out),
            Some(r#"{"class":"func_door","keyValues":{"health":50}}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert!(!json.contains("keyValues"), "{json}");
        assert!(!fake.calls().iter().any(|call| call.starts_with("set_kv")));
    }

    #[test]
    fn the_write_arm_tails_are_the_c_ones() {
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        out.add(b"{\"a\":1}");
        assert_eq!(
            write_finish("w1", Ok((Doc(4), false)), &mut out, "response too large (use limit)"),
            control::result_ok(
                Some("w1"),
                r#"{"a":1,"doc":{"id":"d0","topSerial":0,"undoDepth":0}}"#
            )
        );

        // A scratch-builder overflow keeps the C's arm message (`:2417` + `:5669-5676`).
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        out.add(b"{\"applied\":{}}");
        assert_eq!(
            write_finish(
                "w2",
                Ok((Doc(4), true)),
                &mut out,
                "response too large (use fewer keyvalues)"
            ),
            control::error(Some("w2"), -32603, "response too large (use fewer keyvalues)")
        );

        // A result that overflowed its own buffer answers the arm's message.
        let mut buf = vec![0u8; 4];
        let mut out = Sb::new(&mut buf);
        out.add(b"{\"a\":1}");
        assert!(out.ovf());
        assert_eq!(
            write_finish("w3", Ok((Doc(4), false)), &mut out, "response too large (use limit)"),
            control::error(Some("w3"), -32603, "response too large (use limit)")
        );

        // An op error goes through the C's `build_error_ext`.
        let mut buf = vec![0u8; 64];
        let mut out = Sb::new(&mut buf);
        let error = OpErr::new(-32018, "DocumentNotEditable", "node is locked");
        assert_eq!(
            write_finish("w4", Err(error.clone()), &mut out, "response too large (use limit)"),
            control::error_ext(
                Some("w4"),
                -32018,
                "DocumentNotEditable",
                "{\"reason\":\"node is locked\"}"
            )
        );
    }

    #[test]
    fn entity_create_runs_the_class_lookup_before_the_group() {
        // A class the registry does not know: nothing is created and no group opens (`:2101`).
        let fake = Fake::default();
        fake.script_mut(|s| s.class_exists = false);
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create(w, doc, params, out),
            Some(r#"{"class":"nope"}"#),
        );
        assert_eq!(
            outcome,
            Err(OpErr::new(
                -32006,
                "UnknownClass",
                "not in the CMapNodeFactory registry"
            ))
        );
        assert_eq!(fake.calls(), vec!["class_exists nope"]);

        // The group opens only after the whole target validated, and only the node creation can
        // fail behind it (`:5730-5736`).
        let fake = Fake::default();
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create(w, doc, params, out),
            Some(r#"{"class":"func_door"}"#),
        );
        assert_eq!(
            outcome,
            Err(OpErr::new(
                -32603,
                "InternalError",
                "CreateNodeByName failed (see bridge log)"
            ))
        );
        assert_eq!(
            fake.calls(),
            vec![
                "class_exists func_door".to_string(),
                "undo_open doc=4 label=MCP: create entity".to_string(),
                "create doc=4 class=func_door origin=[0,0,0] angles=[0,0,0] scales=[1,1,1]"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn the_create_body_opens_no_group_for_edit_batch() {
        // `mcp_bridge.c:2167-2171`: the C's `op_entity_create` (what `edit.batch` calls at
        // `:5901`) never opens an undo group — the batch's own single group is open (`:5878`).
        // The arm-shaped wrapper ([`op_entity_create`]) is the only half that opens one.
        let fake = Fake::default().with_node(
            30,
            FakeNode {
                class: "entity",
                id: Some(6),
                ..FakeNode::default()
            },
        );
        fake.script_mut(|s| s.create = Some(30));
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create_no_group(w, doc, params, out),
            Some(r#"{"class":"func_door","origin":[1,2,3]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            format!(
                "{{\"handle\":\"{}\",\"nodeID\":6,\"class\":\"func_door\",\"origin\":[1,2,3],\"scales\":[1,1,1],\"undoLabel\":\"MCP: create entity\"}}",
                json.split("\"handle\":\"").nth(1).and_then(|r| r.split('"').next()).expect("a handle")
            )
        );
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("undo_open")),
            "no group: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn the_set_keyvalues_body_opens_no_group_for_edit_batch() {
        // `mcp_bridge.c:2447-2452`: the C's `op_set_keyvalues` (what `edit.batch` calls at
        // `:5932`) opens no group; the standalone arm (`:5839`) does, and only that wrapper.
        let fake = tree();
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_set_keyvalues_no_group(w, doc, params, out, true),
            Some(r#"{"nodeID":6,"values":{"health":50}}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("undo_open")),
            "no group: {:?}",
            fake.calls()
        );
        // the record still happens (the node is not in the batch's recorded list)
        assert!(
            fake.calls().iter().any(|call| call.starts_with("record hist=")),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    fn entity_create_reaches_the_reply_for_a_created_node() {
        let fake = Fake::default().with_node(
            30,
            FakeNode {
                class: "entity",
                id: Some(6),
                ..FakeNode::default()
            },
        );
        fake.script_mut(|s| s.create = Some(30));
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create(w, doc, params, out),
            Some(r#"{"class":"func_door","origin":[1,2,3],"keyValues":{"health":50}}"#),
        );
        assert_eq!(outcome, Ok(false));
        // The handle is a process-wide token; the rest of the reply is byte-exact.
        let handle = json
            .split("\"handle\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("a handle in the reply");
        assert!(handle.starts_with('n'), "{json}");
        assert_eq!(
            json,
            format!(
                "{{\"handle\":\"{handle}\",\"nodeID\":6,\"class\":\"func_door\",\"origin\":[1,2,3],\"scales\":[1,1,1],\"undoLabel\":\"MCP: create entity\",\"keyValues\":{{\"applied\":{{\"health\":true}},\"warnings\":[]}}}}"
            )
        );
        assert_eq!(
            fake.calls(),
            vec![
                "class_exists func_door".to_string(),
                "undo_open doc=4 label=MCP: create entity".to_string(),
                "create doc=4 class=func_door origin=[1,2,3] angles=[0,0,0] scales=[1,1,1]"
                    .to_string(),
                "properties".to_string(),
                "attr_info health".to_string(),
                "variant health".to_string(),
                "set_kv health=50".to_string(),
            ]
        );
        // the standalone arm opens exactly one group, labelled the C's way (`:5726`)
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.starts_with("undo_open"))
                .cloned()
                .collect::<Vec<_>>(),
            vec!["undo_open doc=4 label=MCP: create entity".to_string()]
        );
    }

    #[test]
    fn a_create_write_back_shows_in_the_reply() {
        // `CreateNodeByName`'s prototype is `float *origin, float *angles`
        // (`s2sdk.h:704`) and the C prints `t->origin` AFTER the call
        // (`mcp_bridge.c:2122-2125`), so a write-back by Hammer is what the caller reads back. The
        // fake writes back what Hammer would: the reply must carry the new values, not the ones
        // the caller sent.
        let fake = Fake::default().with_node(
            30,
            FakeNode {
                class: "entity",
                id: Some(6),
                ..FakeNode::default()
            },
        );
        fake.script_mut(|s| {
            s.create = Some(30);
            s.writeback = Some(([7.0, 8.0, 9.0], [10.0, 20.0, 30.0]));
        });
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_create(w, doc, params, out),
            Some(r#"{"class":"func_door","origin":[1,2,3],"angles":[4,5,6]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert!(json.contains("\"origin\":[7,8,9]"), "{json}");
        assert!(json.contains("\"scales\":[1,1,1]"), "{json}");
        // The call itself still received the caller's arrays (the fake logs them before writing
        // back), and the nodeID read happens above the reply's `keyValues` echo.
        assert_eq!(
            fake.calls(),
            vec![
                "class_exists func_door".to_string(),
                "undo_open doc=4 label=MCP: create entity".to_string(),
                "create doc=4 class=func_door origin=[1,2,3] angles=[4,5,6] scales=[1,1,1]"
                    .to_string(),
            ]
        );
    }

    // -----------------------------------------------------------------------------------------
    // `apply_keyvalues` (`mcp_bridge.c:2313-2424`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_keyvalue_result_is_the_c_byte_sequence() {
        let apply = KvApply {
            entries: vec![
                KvEntry {
                    name: b"a".to_vec(),
                    applied: true,
                    warning: None,
                },
                KvEntry {
                    name: b"b".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::TooLong),
                },
                KvEntry {
                    name: b"c".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::Variant(b"B".to_vec())),
                },
                KvEntry {
                    name: b"d".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::AttrUnreadable),
                },
                KvEntry {
                    name: b"e".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::NumberOutOfRange),
                },
                KvEntry {
                    name: b"f".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::IntOutOfRange),
                },
                KvEntry {
                    name: b"g".to_vec(),
                    applied: false,
                    warning: Some(KvWarning::NotWritable(7)),
                },
            ],
            previous: None,
            stalled: false,
        };
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        assert!(!kv_apply_into(&mut out, &apply, false));
        assert_eq!(
            out.as_cstr(),
            br#"{"applied":{"a":true,"b":false,"c":false,"d":false,"e":false,"f":false,"g":false},"warnings":["b: value longer than 1024 bytes","c: refused, the entity spells this key \"B\" (SetKeyValue matches case-insensitively, `previous` is exact)","d: attribute not readable","e: number out of range for this attribute","f: number out of range for an int attribute","g: attribute type 7 not writable"]}"#
        );

        // `previous` / `previousTruncated` / `valueUndo` (`:2419-2424`).
        let apply = KvApply {
            entries: vec![KvEntry {
                name: b"a".to_vec(),
                applied: true,
                warning: None,
            }],
            previous: Some(vec![
                (b"foo".to_vec(), PrevValue::Str(b"x".to_vec())),
                (b"bar".to_vec(), PrevValue::Int(-3)),
                (b"baz".to_vec(), PrevValue::Float(1.5)),
                (b"q".to_vec(), PrevValue::Cut),
                (b"n".to_vec(), PrevValue::Null),
                (b"t".to_vec(), PrevValue::Bool(true)),
            ]),
            stalled: false,
        };
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        assert!(!kv_apply_into(&mut out, &apply, true));
        assert_eq!(
            out.as_cstr(),
            br#"{"applied":{"a":true},"warnings":[],"previous":{"foo":"x","bar":-3,"baz":1.500,"q":null,"n":null,"t":true},"previousTruncated":["q"],"valueUndo":"recorded"}"#
        );
    }

    #[test]
    fn a_stalled_keyvalue_parser_writes_nothing_and_answers_overflow() {
        let apply = KvApply {
            stalled: true,
            ..applied(b"a")
        };
        let mut buf = vec![0u8; 64];
        let mut out = Sb::new(&mut buf);
        assert!(kv_apply_into(&mut out, &apply, false));
        assert_eq!(out.bytes(), b"");
    }

    #[test]
    fn the_keyvalue_lists_overflow_at_the_c_buffer_sizes() {
        // 900 long names cannot fit the C's 8192-byte `abuf` (`:2314`).
        let entries: Vec<KvEntry> = (0..900)
            .map(|index| KvEntry {
                name: format!("keyvalue_name_{index:04}").into_bytes(),
                applied: true,
                warning: None,
            })
            .collect();
        let apply = KvApply {
            entries,
            previous: None,
            stalled: false,
        };
        let mut buf = vec![0u8; 65536];
        let mut out = Sb::new(&mut buf);
        assert!(kv_apply_into(&mut out, &apply, false), "abuf must overflow");
    }

    // -----------------------------------------------------------------------------------------
    // The result builders of the remaining writes
    // -----------------------------------------------------------------------------------------

    fn sample_conn() -> Conn {
        Conn {
            output: b"OnTrigger".to_vec(),
            target_type: 7,
            target: b"t".to_vec(),
            input: b"Use".to_vec(),
            param: Vec::new(),
            delay: 0.5,
            times_to_fire: 1,
            cut: false,
        }
    }

    #[test]
    fn add_connections_reply_is_the_c_byte_sequence() {
        let results = vec![
            ConnAdd {
                ok: true,
                reported: Some(0),
                conn: Some(sample_conn()),
            },
            ConnAdd {
                ok: false,
                reported: Some(2),
                conn: None,
            },
            ConnAdd {
                ok: false,
                reported: None,
                conn: None,
            },
        ];
        let mut buf = vec![0u8; 8192];
        let mut out = Sb::new(&mut buf);
        assert!(!add_connections_into(&mut out, 6, &results));
        assert_eq!(
            out.as_cstr(),
            br#"{"nodeID":6,"added":[{"index":0,"outputName":"OnTrigger","targetType":7,"targetName":"t","inputName":"Use","overrideParam":"","delay":0.500,"timesToFire":1},{"readback":false,"index":2},{"readback":false}],"count":1,"landed":[{"index":0,"landed":true},{"index":2,"landed":false},{"index":null,"landed":false}],"undoLabel":"MCP: add connections"}"#
        );
    }

    #[test]
    fn remove_connections_reply_is_the_c_byte_sequence() {
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        remove_connections_into(&mut out, 6, &[3, 1, 0], 5);
        assert_eq!(
            out.as_cstr(),
            br#"{"nodeID":6,"removed":[0,1,3],"count":3,"remaining":2,"undoLabel":"MCP: remove connections"}"#
        );
    }

    #[test]
    fn node_delete_and_set_transform_replies_are_the_c_byte_sequence() {
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        node_delete_into(&mut out, &[4, 5]);
        assert_eq!(
            out.as_cstr(),
            br#"{"deleted":[4,5],"undoLabel":"MCP: delete nodes"}"#
        );

        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        set_transform_into(
            &mut out,
            6,
            "entity",
            Some([0.0, 0.0, 0.0]),
            None,
            Some([1.0, 1.0, 1.0]),
        );
        assert_eq!(
            out.as_cstr(),
            br#"{"nodeID":6,"class":"entity","origin":[0,0,0],"angles":null,"scales":[1,1,1],"undoLabel":"MCP: set transform"}"#
        );
    }

    #[test]
    fn the_doc_tail_splices_before_the_closing_brace() {
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        out.add(b"{\"a\":1}");
        doc_tail_into(&mut out, Some(Doc(4)));
        assert_eq!(
            out.as_cstr(),
            br#"{"a":1,"doc":{"id":"d0","topSerial":0,"undoDepth":0}}"#
        );

        // A shape surprise leaves the result untouched (`:1884`).
        let mut buf = vec![0u8; 64];
        let mut out = Sb::new(&mut buf);
        out.add(b"[1]");
        doc_tail_into(&mut out, Some(Doc(4)));
        assert_eq!(out.as_cstr(), b"[1]");
    }

    #[test]
    fn undo_replies_carry_the_c_result_shapes() {
        let doc = r#"{"id":"d1","topSerial":4,"undoDepth":1}"#;
        assert_eq!(
            undo_reply("u1", false, None, doc),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"u1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"undone\":false,\"name\":null,\"doc\":{doc}}}}}\n"
            )
        );
        assert_eq!(
            undo_reply("u2", true, None, doc),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"u2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"redone\":false,\"name\":null,\"doc\":{doc}}}}}\n"
            )
        );
        assert_eq!(
            undo_reply("u3", false, Some((b"MCP: move", true, 1)), doc),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"u3\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"undone\":true,\"name\":\"MCP: move\",\"undoDepth\":1,\"doc\":{doc}}}}}\n"
            )
        );
        assert_eq!(
            undo_reply("u4", true, Some((b"MCP: move", false, 0)), doc),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"u4\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"redone\":false,\"name\":\"MCP: move\",\"redoDepth\":0,\"doc\":{doc}}}}}\n"
            )
        );
    }

    // -----------------------------------------------------------------------------------------
    // The write gate (`mcp_bridge.c:5615-5645`): the document, the edit preconditions, expectDoc
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_write_gate_is_the_c_check_order() {
        // The module reason wins first, then the edit set, then expectDoc.
        let mut fake = Fake::default();
        fake.module_reason = Some("hammer_verifying");
        assert_eq!(
            write_gate(&fake, None),
            Err(OpErr::new(
                -32003,
                "NoActiveDocument",
                "hammer_verifying"
            ))
        );

        let mut fake = Fake::default();
        fake.active = false;
        assert_eq!(
            write_gate(&fake, None),
            Err(OpErr::new(-32018, "DocumentNotEditable", "inactive"))
        );

        let fake = Fake::default();
        assert_eq!(
            write_gate(&fake, None),
            Err(OpErr::new(-32004, "DocumentChanged", "missing"))
        );
        let params = br#"{"expectDoc":{"id":"d0","epoch":0}}"#;
        assert_eq!(write_gate(&fake, Some(params)), Ok(Doc(4)));
        let stale = br#"{"expectDoc":{"id":"d0","epoch":3}}"#;
        assert_eq!(
            write_gate(&fake, Some(stale)),
            Err(OpErr::new(-32004, "DocumentChanged", "epoch"))
        );
    }

    // -----------------------------------------------------------------------------------------
    // `undo.undo` / `undo.redo` (`mcp_bridge.c:5747-5812`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn undo_steps_are_the_c_gates_and_pop() {
        // An empty stack is a RESULT (`:5768-5780`): the active history is read and nothing pops.
        let fake = Fake::default();
        assert_eq!(undo_step(&fake, Doc(4), false), Ok(None));
        assert_eq!(
            fake.calls(),
            vec!["active_history".to_string(), "history_info".to_string()]
        );

        // redo with no readable pair partner: -32017, before any stack read (`:5757-5766`).
        let fake = Fake::default();
        fake.script_mut(|s| s.redo = 0);
        assert_eq!(
            undo_step(&fake, Doc(4), true),
            Err(OpErr::with_data(-32017, "PrerequisiteFailed", |db| {
                db.add(b"{\"reason\":\"bad_redo_history\"}");
            }))
        );
        let fake = Fake::default();
        fake.script_mut(|s| s.history_ok = false);
        assert_eq!(
            undo_step(&fake, Doc(4), true),
            Err(OpErr::with_data(-32017, "PrerequisiteFailed", |db| {
                db.add(b"{\"reason\":\"bad_redo_history\"}");
            }))
        );

        // A non-empty stack: the label is read before the pop, the depth after (`:5781-5790`).
        let fake = Fake::default();
        fake.script_mut(|s| {
            s.history = vec![
                HistoryInfo {
                    name: b"MCP: move".to_vec(),
                    depth: 2,
                    serial: 4,
                },
                HistoryInfo {
                    name: b"MCP: create entity".to_vec(),
                    depth: 1,
                    serial: 3,
                },
            ];
        });
        assert_eq!(
            undo_step(&fake, Doc(4), false),
            Ok(Some(UndoStep {
                name: b"MCP: move".to_vec(),
                undone: true,
                depth: 1,
            }))
        );
        assert_eq!(
            fake.calls(),
            vec![
                "active_history".to_string(),
                "history_info".to_string(),
                "undo_pop 7".to_string(),
                "history_info".to_string(),
            ]
        );

        // The redo half reads the pair partner first (`:5773`, `:5774`, `:5782`): the C's guard is
        // `!GetRedoHistory(doc) || !obj_is(GetRedoHistory(doc), ...)`, so the non-null partner is
        // fetched once for the null check, once for the class check, and once for the stack pick.
        let fake = Fake::default();
        fake.script_mut(|s| {
            s.history = vec![HistoryInfo {
                name: b"MCP: move".to_vec(),
                depth: 2,
                serial: 4,
            }];
        });
        assert_eq!(
            undo_step(&fake, Doc(4), true),
            Ok(Some(UndoStep {
                name: b"MCP: move".to_vec(),
                // The depth did not shrink: `undone:false`, the C's `depth1 < depth0`.
                undone: false,
                depth: 2,
            }))
        );
        assert_eq!(
            fake.calls(),
            vec![
                "redo_history".to_string(),
                "redo_history".to_string(),
                "history_ok 8".to_string(),
                "redo_history".to_string(),
                "history_info".to_string(),
                "undo_pop 8".to_string(),
                "history_info".to_string(),
            ]
        );
    }

    // -----------------------------------------------------------------------------------------
    // `entity.setKeyValues` (`mcp_bridge.c:2470-2541`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn set_keyvalues_param_errors_are_the_c_codes_and_messages() {
        let fake = tree();
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "handle or nodeID required")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "handle or nodeID required")),
            (
                Some(r#"{"handle":"n1","nodeID":6}"#),
                OpErr::new(-32602, "InvalidParams", "handle and nodeID are mutually exclusive"),
            ),
            (
                Some(r#"{"nodeID":"x"}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "nodeID: a non-negative int up to 2147483647 is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6}"#),
                OpErr::new(-32602, "InvalidParams", "values"),
            ),
            (
                Some(r#"{"nodeID":6,"values":5}"#),
                OpErr::new(-32602, "InvalidParams", "values"),
            ),
            (
                Some(r#"{"nodeID":99,"values":{}}"#),
                OpErr::new(
                    -32022,
                    "NodeNotFound",
                    "no node with this nodeID in the live document",
                ),
            ),
            (
                Some(r#"{"nodeID":5,"values":{}}"#),
                OpErr::new(-32602, "InvalidParams", "not an entity"),
            ),
            (
                Some(r#"{"handle":"nope","values":{}}"#),
                OpErr::new(-32005, "StaleHandle", "unknown_handle"),
            ),
        ];
        for (params, expected) in cases {
            let (outcome, _) = run_write(
                &fake,
                |w, doc, params, out| op_set_keyvalues(w, doc, params, out, true),
                *params,
            );
            assert_eq!(outcome, Err(expected.clone()), "{params:?}");
        }
        // The two cases whose node resolved (`nodeID:6` with no `values`, `nodeID:5`) read the
        // node's lock flag first, exactly as `set_keyvalues_target` does (`:2504-2518`); every
        // other refusal happened before any game call.
        assert_eq!(
            fake.calls(),
            vec!["node_locked".to_string(), "node_locked".to_string()]
        );
    }

    #[test]
    fn set_keyvalues_applies_through_the_c_paths() {
        let fake = tree();
        fake.script_mut(|s| {
            s.attrs.insert(
                b"health".to_vec(),
                AttrScript {
                    kind: AT_AT_INT as i32,
                    previous: PrevValue::Int(100),
                    ..AttrScript::default()
                },
            );
            s.attrs.insert(
                b"model".to_vec(),
                AttrScript {
                    kind: AT_AT_STRING as i32,
                    previous: PrevValue::Str(b"old.mdl".to_vec()),
                    variant: Some(b"Model".to_vec()),
                    ..AttrScript::default()
                },
            );
        });
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_set_keyvalues(w, doc, params, out, true),
            Some(r#"{"nodeID":6,"values":{"health":50,"model":"new.mdl"}}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            r#"{"applied":{"health":true,"model":false},"warnings":["model: refused, the entity spells this key \"Model\" (SetKeyValue matches case-insensitively, `previous` is exact)"],"previous":{"health":100,"model":"old.mdl"},"previousTruncated":[],"valueUndo":"recorded"}"#
        );
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "properties".to_string(),
                "undo_open doc=4 label=MCP: set keyvalues".to_string(),
                "active_history".to_string(),
                "record hist=7 node=21".to_string(),
                "previous health".to_string(),
                "previous model".to_string(),
                "attr_info health".to_string(),
                "set_int 50".to_string(),
                "attr_info model".to_string(),
                "variant model".to_string(),
                "modified doc=4".to_string(),
            ]
        );
    }

    // -----------------------------------------------------------------------------------------
    // `entity.addConnections` / `entity.removeConnections`
    // -----------------------------------------------------------------------------------------

    #[test]
    fn add_connections_param_errors_are_the_c_codes_and_messages() {
        let fake = tree();
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (
                Some(r#"{"nodeID":-1}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "nodeID: a non-negative int up to 2147483647 is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "connections: [{output, target, input, param?, delay?, timesToFire?}] required",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"connections":5}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "connections: [{output, target, input, param?, delay?, timesToFire?}] required",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"connections":[]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "connections: 1..64 entries of {output, target, input, param?, delay?, timesToFire?}",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"connections":[{"output":"a","target":"b"}]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "connections: 1..64 entries of {output, target, input, param?, delay?, timesToFire?}",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"connections":[{"output":"a","target":"b","input":"c","delay":-1}]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "connections: 1..64 entries of {output, target, input, param?, delay?, timesToFire?}",
                ),
            ),
            (
                Some(r#"{"nodeID":99,"connections":[{"output":"a","target":"b","input":"c"}]}"#),
                OpErr::new(
                    -32022,
                    "NodeNotFound",
                    "no node with this nodeID in the live document",
                ),
            ),
        ];
        for (params, expected) in cases {
            let (outcome, _) = run_write(
                &fake,
                |w, doc, params, out| op_entity_add_connections(w, doc, params, out),
                *params,
            );
            assert_eq!(outcome, Err(expected.clone()), "{params:?}");
        }
        assert_eq!(fake.calls(), Vec::<String>::new(), "validation precedes the game");
    }

    #[test]
    fn add_connections_runs_the_c_recipe_and_reads_back() {
        let fake = tree();
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_add_connections(w, doc, params, out),
            Some(r#"{"nodeID":6,"connections":[{"output":"OnTrigger","target":"t","input":"Use","delay":0.5,"timesToFire":1}]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            r#"{"nodeID":6,"added":[{"index":0,"outputName":"OnTrigger","targetType":7,"targetName":"t","inputName":"Use","overrideParam":"","delay":0.500,"timesToFire":1}],"count":1,"landed":[{"index":0,"landed":true}],"undoLabel":"MCP: add connections"}"#
        );
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "conn_count".to_string(),
                "undo_open doc=4 label=MCP: add connections".to_string(),
                "record hist=7 node=21".to_string(),
                "add output=OnTrigger target=t input=Use param= delay=0.5 times=1".to_string(),
                "conn_count".to_string(),
                "conn_entry 0".to_string(),
                "modified doc=4".to_string(),
            ]
        );
    }

    #[test]
    fn add_connections_refuses_to_cross_the_read_back_cap_before_the_group() {
        let fake = tree();
        fake.script_mut(|s| s.conns = vec![sample_conn(); RM_INDEX_MAX]);
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_entity_add_connections(w, doc, params, out),
            Some(r#"{"nodeID":6,"connections":[{"output":"a","target":"b","input":"c"}]}"#),
        );
        assert_eq!(
            outcome,
            Err(OpErr::new(
                -32602,
                "InvalidParams",
                "connections: this entity has room for fewer than the requested entries (max 512 live)",
            ))
        );
        // The refusal came before `undo_open`: no empty group is left behind (`:3742-3750`).
        assert_eq!(
            fake.calls(),
            vec!["node_locked".to_string(), "conn_count".to_string()]
        );
    }

    #[test]
    fn remove_connections_param_errors_are_the_c_codes_and_messages() {
        let fake = tree();
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (
                Some(r#"{"nodeID":6}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "exactly one of indices / match is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"indices":[0],"match":{"target":"t"}}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "exactly one of indices / match is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"indices":[]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "indices: 1..512 distinct non-negative ints up to 2147483647",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"indices":[0,0]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "indices: 1..512 distinct non-negative ints up to 2147483647",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"match":5}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: {output?, target?, input?} required",
                ),
            ),
            // The C's own quirk: an empty object never reaches the "at least one" check, because
            // the first `kv_next` sees `}` and answers 0 (`:3871-3876`).
            (
                Some(r#"{"nodeID":6,"match":{}}"#),
                OpErr::new(-32602, "InvalidParams", "match: malformed"),
            ),
            (
                Some(r#"{"nodeID":6,"match":{"x":"y"}}"#),
                OpErr::new(-32602, "InvalidParams", "match: only output, target and input"),
            ),
            (
                Some(r#"{"nodeID":6,"match":{"output":""}}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: output/target/input are 1..255 bytes without control characters",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"match":{"output":"a\nb"}}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: output/target/input are 1..255 bytes without control characters",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"match":{"input":5}}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "match: output/target/input are 1..255 bytes without control characters",
                ),
            ),
            (
                Some(r#"{"nodeID":99,"match":{"target":"t"}}"#),
                OpErr::new(
                    -32022,
                    "NodeNotFound",
                    "no node with this nodeID in the live document",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"indices":[3]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "indices: 3 is out of range (the entity has 0 connection(s))",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"match":{"target":"t"}}"#),
                OpErr::new(
                    -32026,
                    "ConnectionNotFound",
                    "no live connection matches the match filter",
                ),
            ),
        ];
        for (params, expected) in cases {
            let (outcome, _) = run_write(
                &fake,
                |w, doc, params, out| op_entity_remove_connections(w, doc, params, out),
                *params,
            );
            assert_eq!(outcome, Err(expected.clone()), "{params:?}");
        }
        // Only the two cases that passed the whole parse read the node (the lock flag and the
        // vector, `:3900-3906`); nothing else reached the game.
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "conn_count".to_string(),
                "node_locked".to_string(),
                "conn_count".to_string(),
            ]
        );
    }

    #[test]
    fn remove_connections_removes_descending_and_reports_ascending() {
        let fake = tree();
        fake.script_mut(|s| {
            s.conns = vec![sample_conn(), sample_conn(), sample_conn()];
        });
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_remove_connections(w, doc, params, out),
            Some(r#"{"nodeID":6,"indices":[0,2]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            r#"{"nodeID":6,"removed":[0,2],"count":2,"remaining":1,"undoLabel":"MCP: remove connections"}"#
        );
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "conn_count".to_string(),
                "undo_open doc=4 label=MCP: remove connections".to_string(),
                "record hist=7 node=21".to_string(),
                "remove 2".to_string(),
                "remove 0".to_string(),
                "modified doc=4".to_string(),
            ]
        );

        // A `match` resolves through the live vector, in ascending order (`:3925-3958`).
        let fake = tree();
        fake.script_mut(|s| {
            s.conns = vec![sample_conn(), sample_conn()];
        });
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_entity_remove_connections(w, doc, params, out),
            Some(r#"{"nodeID":6,"match":{"target":"t"}}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            r#"{"nodeID":6,"removed":[0,1],"count":2,"remaining":0,"undoLabel":"MCP: remove connections"}"#
        );
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "conn_count".to_string(),
                "conn_filter 0".to_string(),
                "conn_filter 1".to_string(),
                "undo_open doc=4 label=MCP: remove connections".to_string(),
                "record hist=7 node=21".to_string(),
                "remove 1".to_string(),
                "remove 0".to_string(),
                "modified doc=4".to_string(),
            ]
        );
    }

    #[test]
    fn a_refused_removal_is_a_maybe_executed_error() {
        let fake = tree();
        fake.script_mut(|s| {
            s.conns = vec![sample_conn(), sample_conn()];
            s.remove_ok = false;
        });
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_entity_remove_connections(w, doc, params, out),
            Some(r#"{"nodeID":6,"indices":[0]}"#),
        );
        assert_eq!(
            outcome,
            Err(OpErr::maybe(
                -32603,
                "InternalError",
                "RemoveConnection refused an index; the removals before it in this group already ran - verify with entity.get, do not retry",
            ))
        );
    }

    // -----------------------------------------------------------------------------------------
    // `node.delete` / `node.setTransform`
    // -----------------------------------------------------------------------------------------

    /// The tree plus a non-entity child (id 3, class "world") - the only way to reach the C's
    /// "cannot be deleted" and "not an entity or mesh" branches, since `geo_find` never matches an
    /// id against the world node itself (`mcp_bridge.c:3216-3224`).
    fn tree_with_world_child() -> Fake {
        let mut fake = tree();
        fake.nodes.get_mut(&10).expect("the world node").children = Some(vec![20, 21, 22, 23]);
        fake.with_node(
            23,
            FakeNode {
                class: "world",
                id: Some(3),
                ..FakeNode::default()
            },
        )
    }

    #[test]
    fn node_delete_param_errors_are_the_c_codes_and_messages() {
        let fake = tree_with_world_child();
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "nodeIDs: [int,...] required")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "nodeIDs: [int,...] required")),
            (
                Some(r#"{"nodeIDs":5}"#),
                OpErr::new(-32602, "InvalidParams", "nodeIDs: [int,...] required"),
            ),
            (Some(r#"{"nodeIDs":[]}"#), OpErr::new(-32602, "InvalidParams", "nodeIDs: empty")),
            (
                Some(r#"{"nodeIDs":[-1]}"#),
                OpErr::new(-32602, "InvalidParams", "nodeIDs: up to 256 non-negative ints"),
            ),
            (
                Some(r#"{"nodeIDs":[1.5]}"#),
                OpErr::new(-32602, "InvalidParams", "nodeIDs: up to 256 non-negative ints"),
            ),
            (
                Some(r#"{"nodeIDs":[3000000000]}"#),
                OpErr::new(-32602, "InvalidParams", "nodeIDs: up to 256 non-negative ints"),
            ),
            (
                Some(r#"{"nodeIDs":[1 2]}"#),
                OpErr::new(-32602, "InvalidParams", "nodeIDs: malformed"),
            ),
            (
                Some(r#"{"nodeIDs":[99]}"#),
                OpErr::new(-32022, "NodeNotFound", "nodeID 99 not found"),
            ),
            (
                Some(r#"{"nodeIDs":[3]}"#),
                OpErr::new(-32602, "InvalidParams", "nodeID 3 cannot be deleted (world)"),
            ),
        ];
        for (params, expected) in cases {
            let (outcome, _) = run_write(
                &fake,
                |w, doc, params, out| op_node_delete(w, doc, params, out),
                *params,
            );
            assert_eq!(outcome, Err(expected.clone()), "{params:?}");
        }
        assert_eq!(fake.calls(), Vec::<String>::new(), "validation precedes the game");
    }

    #[test]
    fn node_delete_deletes_every_id_in_one_group() {
        let fake = tree();
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_node_delete(w, doc, params, out),
            Some(r#"{"nodeIDs":[6,7]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(json, r#"{"deleted":[6,7],"undoLabel":"MCP: delete nodes"}"#);
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "node_locked".to_string(),
                "undo_open doc=4 label=MCP: delete nodes".to_string(),
                "delete doc=4 node=21".to_string(),
                "delete doc=4 node=22".to_string(),
            ]
        );
    }

    #[test]
    fn node_set_transform_param_errors_are_the_c_codes_and_messages() {
        let fake = tree_with_world_child();
        let cases: &[(Option<&str>, OpErr)] = &[
            (None, OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (Some("{}"), OpErr::new(-32602, "InvalidParams", "nodeID required")),
            (
                Some(r#"{"nodeID":-1,"origin":[0,0,0]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "nodeID: a non-negative int up to 2147483647 is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "at least one of origin, angles, scales is required",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"origin":[1,2]}"#),
                OpErr::new(-32602, "InvalidParams", "origin: [x,y,z] required"),
            ),
            (
                Some(r#"{"nodeID":6,"angles":5}"#),
                OpErr::new(-32602, "InvalidParams", "angles: [pitch,yaw,roll] required"),
            ),
            (
                Some(r#"{"nodeID":6,"scales":"x"}"#),
                OpErr::new(-32602, "InvalidParams", "scales: [x,y,z] required"),
            ),
            (
                Some(r#"{"nodeID":6,"origin":[1048577,0,0]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "origin: each |component| <= 1048576",
                ),
            ),
            (
                Some(r#"{"nodeID":6,"scales":[0,1,1]}"#),
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "scales: each finite and in (0, 1000]",
                ),
            ),
            (
                Some(r#"{"nodeID":3,"origin":[0,0,0]}"#),
                OpErr::new(-32602, "InvalidParams", "not an entity or mesh"),
            ),
            (
                Some(r#"{"nodeID":99,"origin":[0,0,0]}"#),
                OpErr::new(
                    -32022,
                    "NodeNotFound",
                    "no node with this nodeID in the live document",
                ),
            ),
        ];
        for (params, expected) in cases {
            let (outcome, _) = run_write(
                &fake,
                |w, doc, params, out| op_node_set_transform(w, doc, params, out),
                *params,
            );
            assert_eq!(outcome, Err(expected.clone()), "{params:?}");
        }
        assert_eq!(fake.calls(), Vec::<String>::new(), "validation precedes the game");
    }

    #[test]
    fn node_set_transform_writes_scales_angles_origin_in_that_order() {
        let fake = tree();
        let (outcome, json) = run_write(
            &fake,
            |w, doc, params, out| op_node_set_transform(w, doc, params, out),
            Some(r#"{"nodeID":6,"origin":[1,2,3],"angles":[4,5,6],"scales":[2,2,2]}"#),
        );
        assert_eq!(outcome, Ok(false));
        assert_eq!(
            json,
            r#"{"nodeID":6,"class":"entity","origin":[0,0,0],"angles":[0,0,0],"scales":[1,1,1],"undoLabel":"MCP: set transform"}"#
        );
        assert_eq!(
            fake.calls(),
            vec![
                "node_locked".to_string(),
                "undo_open doc=4 label=MCP: set transform".to_string(),
                "record hist=7 node=21".to_string(),
                "set_scales [2,2,2]".to_string(),
                "set_angles [4,5,6]".to_string(),
                "set_origin [1,2,3]".to_string(),
                "modified doc=4".to_string(),
                "angles".to_string(),
                "scales".to_string(),
            ]
        );

        // A locked node is refused before the group opens (`:5584`).
        let fake = tree();
        fake.script_mut(|s| s.locked = true);
        let (outcome, _) = run_write(
            &fake,
            |w, doc, params, out| op_node_set_transform(w, doc, params, out),
            Some(r#"{"nodeID":6,"origin":[1,2,3]}"#),
        );
        assert_eq!(
            outcome,
            Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked"))
        );
        assert_eq!(fake.calls(), vec!["node_locked".to_string()]);
    }
}
