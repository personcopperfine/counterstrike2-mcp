//! The ModelDoc document ops: `modeldoc.active`, `modeldoc.open`, `modeldoc.reload`,
//! `modeldoc.save`, `modeldoc.compile`, `modeldoc.nodeTypes` (M4 lane MODELDOC) - the C's
//! `op_modeldoc` (`mcp_bridge.c:5098-5391`) without `modeldoc.render` (the render lane,
//! `mcp_bridge_modeldoc_render.inc`: it stays unregistered, so `modeldoc.render` keeps
//! its `-32011 NotImplemented` answer until its own lane ports it).
//!
//! # How it is split
//!
//! The result/error builders are plain functions over plain inputs ([`MdFacts`], the
//! [`NodeTypes`] rows), held byte for byte to the C's `sb_add` sequences by the unit tests.
//! Everything that reads game memory or calls into the game sits behind the [`Chain`] trait,
//! whose production implementation is [`win::LiveModelDoc`] (the Windows tools build only: the
//! generated modeldoc modules are `cfg(windows)`). On a non-Windows host there is no
//! modeldoc_editor.dll at all: the method gate (`-32017`) answers before a body starts, and the
//! [`NoModelDoc`] fallback that still reaches a body fails loudly instead of inventing an
//! answer - except the reasons the C itself answers without a module (`modeldoc_not_loaded`,
//! `modeldoc_unavailable`), which are the C's own behaviour for a machine without the module.
//!
//! # Statics
//!
//! The C keeps `g_modeldoc` and `g_mdCompile`/`g_mdCompileBound` in process-wide statics
//! (`mcp_bridge.c:5111-5113`); all of it is touched on the tools main thread only - the bind
//! happens inside `resolve_modeldoc`, and every body runs in the tick's drain. The Rust port
//! keeps the same shape with `Mutex`es; no other thread calls into this module.

use crate::entity_json::{json_find, json_string, Sb};
use crate::generated::pins;
use crate::log;
use crate::ops::ident::mem_ok;
use crate::ops::{finish, Handler, OpErr};
use crate::queue::Request;

/// The modeldoc arm's result buffer (`mcp_bridge.c:6206`, `static char rbuf[256 * 1024]`).
pub(crate) const RESULT_BUF: usize = 256 * 1024;
/// The modeldoc arm's overflow answer (`mcp_bridge.c:6212`).
pub(crate) const MODELDOC_OVERFLOW: &str = "InternalError: modeldoc result too large";
/// `md_doc_json`'s / `modeldoc.reload`'s / `modeldoc.save`'s path buffer
/// (`mcp_bridge.c:5183`, `:5287`, `:5314`).
const PATH_CAP: usize = 1024;
/// `modeldoc.nodeTypes`' per-descriptor name buffer (`mcp_bridge.c:5375`).
#[allow(dead_code)] // the Windows reader and the host tests are the callers
const NAME_CAP: usize = 128;

// ---------------------------------------------------------------------------------------------
// The C string helpers (`cstr`, `md_str`)
// ---------------------------------------------------------------------------------------------

/// The C string view of `bytes`: everything before the first NUL.
fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&byte| byte == 0) {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

/// `md_str` (`mcp_bridge.c:5169-5180`): copy a game-owned C string, probing readability first
/// and again at every page boundary (strings can point at freed or reallocated CUtlString
/// storage). Returns 1 when the whole string fit, -1 when it was cut at cap-1 (out holds the
/// prefix), 0 (empty out) when null or unreadable.
///
/// This is this module's own copy of the C helper (the pet lane ports its own; the orchestrator
/// dedups at merge). It is deliberately NOT `hammer.rs`'s `probe_str`: the C's `md_str` does
/// neither the exact-fit check nor the UTF-8-boundary cut (`:1971-1989`), so a cut name keeps
/// its raw cap-1 prefix here, exactly as the C leaves it.
///
/// # Safety
///
/// `s` is a game pointer: it is only ever *read* through [`mem_ok`], which reports an
/// unreadable address instead of faulting on both targets.
#[allow(dead_code)] // the Windows reader and the host tests are the callers
unsafe fn md_str(s: *const u8, out: &mut [u8]) -> i32 {
    out[0] = 0;
    if s.is_null() || !mem_ok(s.cast()) {
        return 0;
    }
    let cap = out.len();
    let mut i = 0usize;
    while i + 1 < cap {
        // `mcp_bridge.c:5174`: re-probe at every 4 KiB page boundary of the walk.
        if i != 0
            && s.wrapping_add(i) as usize & 0xFFF == 0
            && !mem_ok(s.wrapping_add(i).cast())
        {
            out[0] = 0;
            return 0;
        }
        // SAFETY: the first byte was proven readable above, and every page boundary of the
        // walk is re-probed; the C reads the same bytes (`mcp_bridge.c:5176`).
        let byte = unsafe { s.add(i).read() };
        out[i] = byte;
        if byte == 0 {
            return 1;
        }
        i += 1;
    }
    out[i] = 0;
    -1
}

// ---------------------------------------------------------------------------------------------
// The reader contract and the object chain (`mcp_bridge.c:5111-5167`)
// ---------------------------------------------------------------------------------------------

/// A node-type row: the descriptor's `typeId` and its className's bytes, as the C's bounded
/// copy hands them to `sb_addjson` (`mcp_bridge.c:5374-5384`). Raw bytes, not text:
/// [`Sb::addjson`] does the C's invalid-UTF-8 replacement.
pub(crate) type NodeTypeRow = (u32, Vec<u8>);

/// `modeldoc.nodeTypes`' table read (`mcp_bridge.c:5353-5384`): either the rows, or the first
/// refusal (the arm's `-32017 PrerequisiteFailed` reasons).
#[allow(dead_code)] // constructed by the Windows reader and by the host builder tests
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NodeTypes {
    /// The table global is unusable; the count for the error text (the C prints
    /// `count < 0 ? 0 : count`).
    Unusable(i64),
    /// A descriptor or its array slot is unreadable (`mcp_bridge.c:5370-5372`).
    BadDescriptor,
    /// A descriptor's className did not copy whole (`mcp_bridge.c:5376-5378`).
    BadName,
    /// `count` rows in table order.
    Rows(Vec<NodeTypeRow>),
}

/// The doc object's fields, read from the live session by [`Chain::md_facts`] - the inputs of
/// `md_doc_json` (`mcp_bridge.c:5182-5194`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MdFacts {
    /// `md_str(GetPath(session))`'s C-string bytes (possibly the cut prefix; the C ignores the
    /// copy's status here).
    pub path: Vec<u8>,
    /// `IsModified(session)`.
    pub dirty: bool,
    /// `session->isNewFile`.
    pub is_new: bool,
    /// `session->asset != 0`.
    pub has_asset: bool,
}

/// The live ModelDoc state the six ops need, one method per C hop.
///
/// Every method is one unguarded read on an object the previous hop already identity-checked
/// (`obj_is`) or one identity check itself, exactly as the C's pointer chain does it
/// (`mcp_bridge.c:5111-5167`). A fake implementation drives the reason chain in the unit tests.
pub(crate) trait Chain {
    /// `mod_ok(PIN_MODELDOC)`'s refusal reason: `None` when verified, else the C's
    /// `modeldoc_mismatch` / `modeldoc_verifying` / `modeldoc_not_loaded`
    /// (`mcp_bridge.c:5149-5154`).
    fn module_reason(&self) -> Option<&'static str>;
    /// `resolve_modeldoc()` (`mcp_bridge.c:5124-5145`): the `CModelDocEditorApp *`, or `None`
    /// after the hop's own log line.
    fn app(&self) -> Option<usize>;
    /// `CModelDocEditorApp_ActiveSession` (`s2sdk.h:557`): may be null.
    fn session(&self, app: usize) -> Option<usize>;
    /// `obj_is(obj, PIN_MODELDOC, vt)`.
    fn is_of(&self, object: usize, vt: u32) -> bool;
    /// `session->doc` (`s2sdk.h:593`, the generated `m_pDoc`), 0 when unset.
    fn doc(&self, session: usize) -> usize;
    /// `session->isNewFile` (`s2sdk.h:579`, the generated `m_bIsNew` byte).
    fn is_new(&self, session: usize) -> bool;
    /// `session->asset != 0` (`s2sdk.h:583`, the generated `m_pAsset`).
    fn has_asset(&self, session: usize) -> bool;
    /// `md_str(session->vt->GetPath(session), out, cap)` (`s2sdk.h:569`): the status and
    /// the C-string bytes of the copy.
    fn path(&self, session: usize, cap: usize) -> (i32, Vec<u8>);
    /// `session->vt->IsModified(session)` (`s2sdk.h:567`).
    fn is_modified(&self, session: usize) -> bool;
    /// `session->vt->SetModified(session, 0)` (`s2sdk.h:568`).
    fn set_modified(&self, session: usize, modified: u8);
    /// `session->vt->ReloadDocument(session)` (`s2sdk.h:565`).
    fn reload(&self, session: usize);
    /// `session->vt->SaveAndNotify(session, oldPath, newPath, 0, 0)` (`s2sdk.h:573`;
    /// the C passes the same buffer twice, `mcp_bridge.c:5324`).
    fn save(&self, session: usize, old_path: &[u8], new_path: &[u8]) -> bool;
    /// `app->vt->OpenFile(app, path, 1)` (`s2sdk.h:551`; flags 1 = what the UI passes).
    fn open(&self, app: usize, path: &[u8]) -> usize;
    /// `modeldoc_bind(); g_mdCompile(session, 0)` (`mcp_bridge.c:5343-5344`).
    fn compile(&self, session: usize);
    /// `modeldoc.nodeTypes`' table read (`mcp_bridge.c:5353-5384`).
    fn node_types(&self) -> NodeTypes;

    /// `md_doc_json`'s reads (`mcp_bridge.c:5183-5187`), in the C's order: GetPath + `md_str`,
    /// then IsModified, then the isNewFile byte, then the asset pointer.
    fn md_facts(&self, session: usize) -> MdFacts {
        let (_, path) = self.path(session, PATH_CAP);
        MdFacts {
            path,
            dirty: self.is_modified(session),
            is_new: self.is_new(session),
            has_asset: self.has_asset(session),
        }
    }
}

/// `active_modeldoc` (`mcp_bridge.c:5147-5167`): the active session with per-hop validation,
/// or the C's short reason.
pub(crate) fn active_modeldoc_with(md: &dyn Chain) -> Result<usize, &'static str> {
    if let Some(reason) = md.module_reason() {
        return Err(reason);
    }
    let Some(app) = md.app() else {
        return Err("modeldoc_unavailable");
    };
    let Some(session) = md.session(app) else {
        return Err("no_session");
    };
    if !md.is_of(session, pins::MDV_SESSION_VT.value as u32) {
        return Err("bad_session");
    }
    if md.doc(session) == 0 {
        return Err("no_document");
    }
    if !md.is_of(md.doc(session), pins::MDV_DOC_VT.value as u32) {
        return Err("bad_doc");
    }
    Ok(session)
}

// ---------------------------------------------------------------------------------------------
// The result builders (pure over plain inputs)
// ---------------------------------------------------------------------------------------------

/// `md_doc_json` (`mcp_bridge.c:5182-5194`): the doc object shared by `modeldoc.active` /
/// `modeldoc.open`, byte for byte. `compileStatus` is always `null`: the embedded
/// `CModelDocAnalysis` is an IErrorListener, not a CUtlString - seen live as binary garbage
/// when read as `char *`; never read (`mcp_bridge.c:5189-5191`).
fn md_doc_json_into(b: &mut Sb, facts: &MdFacts) {
    b.add(b"{\"path\":"); // :5188
    b.addjson(&facts.path);
    b.add(b",\"dirty\":"); // :5189
    b.add(if facts.dirty { b"true" } else { b"false" });
    b.add(b",\"isNewFile\":"); // :5190
    b.add(if facts.is_new { b"true" } else { b"false" });
    b.add(b",\"hasAsset\":"); // :5191
    b.add(if facts.has_asset { b"true" } else { b"false" });
    b.add(b",\"compileStatus\":null"); // :5192
    b.add(b",\"docType\":\"vmdl\"}"); // :5193
}

/// One `modeldoc.nodeTypes` row (`mcp_bridge.c:5381-5384`).
fn node_type_row_into(b: &mut Sb, row: &NodeTypeRow) {
    b.add(b"{\"typeId\":");
    b.addnum(u64::from(row.0));
    b.add(b",\"className\":");
    b.addjson(&row.1);
    b.add(b"}");
}

/// `modeldoc.nodeTypes`' result object (`mcp_bridge.c:5364-5385`), byte for byte.
fn node_types_into(b: &mut Sb, rows: &[NodeTypeRow]) {
    b.add(b"{\"count\":"); // :5364
    b.addnum(rows.len() as u64);
    b.add(b",\"types\":["); // :5365
    for (i, row) in rows.iter().enumerate() {
        if i != 0 {
            b.add(b","); // :5379
        }
        node_type_row_into(b, row);
    }
    b.add(b"]}"); // :5385
}

// ---------------------------------------------------------------------------------------------
// The op bodies (`mcp_bridge.c:5199-5391`)
// ---------------------------------------------------------------------------------------------

/// `modeldoc.active` (`mcp_bridge.c:5201-5213`): never errors on no doc - `{"doc":null,"reason"}`.
fn op_active_with(md: &dyn Chain, out: &mut Sb) -> Result<(), OpErr> {
    match active_modeldoc_with(md) {
        Err(reason) => {
            out.add(b"{\"doc\":null,\"reason\":"); // :5206
            out.addjson(reason.as_bytes());
            out.add(b"}");
        }
        Ok(session) => {
            out.add(b"{\"doc\":"); // :5209
            md_doc_json_into(out, &md.md_facts(session));
            out.add(b"}"); // :5211
        }
    }
    Ok(())
}

/// `modeldoc.open`'s first refusal (`mcp_bridge.c:5218`).
fn open_params_err() -> OpErr {
    OpErr::new(
        -32602,
        "InvalidParams",
        "path: relative to the addon content root (models/...), 1..512 chars, no '..'",
    )
}

/// `modeldoc.open` (`mcp_bridge.c:5214-5272`). Every refusal before [`Chain::open`] keeps a
/// garbled path away from OpenFile, whose "Cannot open" / "outside the content dir" modals
/// block the main thread until a human clicks (`mcp_bridge.c:5223`).
fn op_open_with(md: &dyn Chain, params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let mut path = [0u8; 520]; // :5215
    let Some(params) = params else {
        return Err(open_params_err());
    };
    if !json_string(params.as_bytes(), b"path", &mut path) || path[0] == 0 {
        return Err(open_params_err()); // :5217-5220
    }
    let path = cstr(&path);
    let pl = path.len(); // :5221
    if pl > 512 {
        return Err(OpErr::new(-32602, "InvalidParams", "path: 1..512 chars")); // :5222
    }
    let bytes = params.as_bytes();
    // The raw JSON text of the value (`json_string` collapses escapes, `:5226`): one backslash
    // anywhere before the closing quote is refused.
    if let Some(raw) = json_find(bytes, b"path") {
        let mut q = raw + 1;
        while q < bytes.len() && bytes[q] != 0 && bytes[q] != b'"' {
            if bytes[q] == b'\\' {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "path: no JSON escapes (use '/' separators)",
                ));
            }
            q += 1;
        }
    }
    for &byte in path {
        if byte < 0x20 || byte == 0x7f {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "path: no control characters",
            )); // :5235
        }
    }
    if path[0] == b'/' || path[0] == b'\\' || (pl > 1 && path[1] == b':') {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "path: relative to the addon content root (models/...), not absolute",
        )); // :5240-5242
    }
    if pl < 6
        || path[pl - 5] != b'.'
        || (path[pl - 4] | 0x20) != b'v'
        || (path[pl - 3] | 0x20) != b'm'
        || (path[pl - 2] | 0x20) != b'd'
        || (path[pl - 1] | 0x20) != b'l'
    {
        return Err(OpErr::new(-32602, "InvalidParams", "path: a .vmdl file")); // :5244-5247
    }
    // Only '..' needs a refusal here: the OpenFile side resolves it, and OpenFile on a file
    // outside the content dir pops a modal (`mcp_bridge.c:5249-5251`).
    for i in 0..pl - 1 {
        if path[i] == b'.' && path[i + 1] == b'.' {
            return Err(OpErr::new(-32602, "InvalidParams", "path: no '..'")); // :5253
        }
    }
    let Some(app) = md.app() else {
        return Err(OpErr::new(
            -32003,
            "NoActiveDocument",
            "modeldoc_unavailable",
        )); // :5257-5258
    };
    let opened = md.open(app, path); // :5259 (flags 1 = what the UI passes)
    log::line("modeldoc: open "); // :5260
    log::line(&String::from_utf8_lossy(path)); // :5261
    log::line(&format!(
        "modeldoc: open rc {}",
        // `lognum("modeldoc: open rc ", (u64)(long long)(size_t)opened)` (`:5262`): the same
        // 64 bits, printed as unsigned decimal.
        (opened as isize) as usize as u64
    ));
    let active = active_modeldoc_with(md); // :5263-5265
    out.add(b"{\"opened\":"); // :5266
    out.add(if opened != 0 { b"true" } else { b"false" });
    out.add(b",\"path\":"); // :5267
    out.addjson(path);
    out.add(b",\"active\":"); // :5268
    match active {
        Ok(session) => md_doc_json_into(out, &md.md_facts(session)), // :5269
        Err(_) => out.add(b"null"),
    }
    out.add(b"}"); // :5270
    Ok(())
}

/// `modeldoc.reload` (`mcp_bridge.c:5273-5305`).
fn op_reload_with(md: &dyn Chain, params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let session = active_modeldoc_with(md)
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :5276
    let dirty = md.is_modified(session); // :5277
    let mut force = false;
    if let Some(params) = params {
        // `json_find(params, "force")` and the value's first byte `t` (`mcp_bridge.c:5280-5281`).
        if let Some(f) = json_find(params.as_bytes(), b"force") {
            force = params.as_bytes().get(f) == Some(&b't');
        }
    }
    if dirty && !force {
        return Err(OpErr::new(
            -32016,
            "UnsavedChanges",
            "document is dirty; pass force:true to discard",
        )); // :5283-5285
    }
    let (_, path) = md.path(session, PATH_CAP); // :5288, capture before the reload
    // ReloadDocument would load "" into an empty document: useless and destructive (`:5289`).
    if md.is_new(session) || path.is_empty() {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no path to reload from",
        )); // :5291
    }
    md.reload(session); // :5293, replaces the CModelDoc: everything below re-resolves
    let active = active_modeldoc_with(md); // :5296-5297
    if active.is_err() {
        log::line("modeldoc: reload left no valid document"); // :5298
    }
    log::line(&format!("modeldoc: reload, dirty was {}", u64::from(dirty))); // :5299
    out.add(b"{\"reloaded\":"); // :5300
    out.add(if active.is_ok() { b"true" } else { b"false" });
    out.add(b",\"path\":"); // :5301
    out.addjson(&path);
    out.add(b",\"discardedChanges\":"); // :5302
    out.add(if dirty { b"true" } else { b"false" });
    out.add(b"}"); // :5303
    Ok(())
}

/// `modeldoc.save` (`mcp_bridge.c:5306-5333`).
fn op_save_with(md: &dyn Chain, out: &mut Sb) -> Result<(), OpErr> {
    let session = active_modeldoc_with(md)
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :5309
    if md.is_new(session) {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no path (Save As would open a modal)",
        )); // :5310-5313
    }
    // Copied BEFORE the save: the path string may be reallocated by SaveAndNotify (`:5314`).
    let (status, path) = md.path(session, PATH_CAP); // :5316
    // Strict: a cut path handed to SaveAndNotify(path, path) would repoint the session (`:5315`).
    if status != 1 || path.is_empty() {
        return Err(OpErr::new(
            -32015,
            "UntitledDocument",
            "no usable path (Save As would open a modal)",
        )); // :5317-5319
    }
    let dirty = md.is_modified(session); // :5321
    let mut saved = true;
    let mut skipped = true;
    if dirty {
        skipped = false; // :5323
        saved = md.save(session, &path, &path); // :5324, the FileSave call
        if saved {
            md.set_modified(session, 0); // :5325, mirrors FileSave
        }
    }
    out.add(b"{\"saved\":"); // :5328
    out.add(if saved { b"true" } else { b"false" });
    out.add(b",\"skippedClean\":"); // :5329
    out.add(if skipped { b"true" } else { b"false" });
    out.add(b",\"path\":"); // :5330
    out.addjson(&path);
    out.add(b"}"); // :5331
    Ok(())
}

/// `modeldoc.compile` (`mcp_bridge.c:5334-5351`).
fn op_compile_with(md: &dyn Chain, out: &mut Sb) -> Result<(), OpErr> {
    let session = active_modeldoc_with(md)
        .map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?; // :5337
    if !md.has_asset(session) {
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "no_asset")); // :5340
    }
    let saved_first = md.is_modified(session); // :5342, Compile saves first when modified
    md.compile(session); // :5343-5344, mode 0; returns nothing useful
    // Compile returns nothing and its status lands in a sink object (see `md_doc_json`):
    // the outcome is unknown here - the tool checks the .vmdl_c mtime (`:5345-5346`).
    out.add(b"{\"status\":null,\"failed\":null"); // :5347
    out.add(b",\"savedFirst\":"); // :5348
    out.add(if saved_first { b"true" } else { b"false" });
    out.add(b"}"); // :5349
    Ok(())
}

/// `modeldoc.nodeTypes` (`mcp_bridge.c:5352-5387`).
fn op_node_types_with(md: &dyn Chain, out: &mut Sb) -> Result<(), OpErr> {
    match md.node_types() {
        NodeTypes::Unusable(count) => {
            // The C builds the reason in a 128-byte scratch SB (`mcp_bridge.c:5357-5361`).
            let mut buf = vec![0u8; 128];
            let mut w = Sb::new(&mut buf);
            w.add(b"node-type table unusable (count ");
            w.addi(if count < 0 { 0 } else { count });
            w.add(b")");
            Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                &String::from_utf8_lossy(w.as_cstr()),
            ))
        }
        NodeTypes::BadDescriptor => Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "node-type descriptor unreadable",
        )),
        NodeTypes::BadName => Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "node-type className unreadable",
        )),
        NodeTypes::Rows(rows) => {
            node_types_into(out, &rows);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// One arm of the C's modeldoc dispatch (`mcp_bridge.c:6205-6219`): the op over a 256 KiB
/// result buffer, `finish` answering the error / `-32603 "InternalError: modeldoc result too
/// large"` / the result envelope.
fn modeldoc_op(req: &Request, op: impl FnOnce(&dyn Chain, &mut Sb) -> Result<(), OpErr>) -> String {
    let mut buf = vec![0u8; RESULT_BUF];
    let mut out = Sb::new(&mut buf);
    let outcome = op(game(), &mut out);
    finish(&req.id, outcome, &out, MODELDOC_OVERFLOW)
}

/// `modeldoc.active`'s fully built reply (`mcp_bridge.c:5201-5213`).
fn modeldoc_active(req: &Request) -> String {
    modeldoc_op(req, |md, out| op_active_with(md, out))
}

/// `modeldoc.open`'s fully built reply (`mcp_bridge.c:5214-5272`).
fn modeldoc_open(req: &Request) -> String {
    modeldoc_op(req, |md, out| op_open_with(md, req.params.as_deref(), out))
}

/// `modeldoc.reload`'s fully built reply (`mcp_bridge.c:5273-5305`).
fn modeldoc_reload(req: &Request) -> String {
    modeldoc_op(req, |md, out| {
        op_reload_with(md, req.params.as_deref(), out)
    })
}

/// `modeldoc.save`'s fully built reply (`mcp_bridge.c:5306-5333`).
fn modeldoc_save(req: &Request) -> String {
    modeldoc_op(req, op_save_with)
}

/// `modeldoc.compile`'s fully built reply (`mcp_bridge.c:5334-5351`).
fn modeldoc_compile(req: &Request) -> String {
    modeldoc_op(req, op_compile_with)
}

/// `modeldoc.nodeTypes`' fully built reply (`mcp_bridge.c:5352-5387`).
fn modeldoc_node_types(req: &Request) -> String {
    modeldoc_op(req, op_node_types_with)
}

/// This group's bodies. One line per method in the C's dispatch chain (`mcp_bridge.c:5201`,
/// `:5214`, `:5273`, `:5306`, `:5334`, `:5352`); `modeldoc.render` is the render lane's and
/// stays unregistered.
pub static HANDLERS: &[(&str, Handler)] = &[
    ("modeldoc.active", modeldoc_active),
    ("modeldoc.open", modeldoc_open),
    ("modeldoc.reload", modeldoc_reload),
    ("modeldoc.save", modeldoc_save),
    ("modeldoc.compile", modeldoc_compile),
    ("modeldoc.nodeTypes", modeldoc_node_types),
];

// ---------------------------------------------------------------------------------------------
// The production reader (Windows tools build only)
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use core::ffi::{c_char, c_int, c_void};
    use core::ptr::read_unaligned;
    use std::sync::{Mutex, MutexGuard};

    use s2sdk_sys::generated::{
        CBaseEditorApp, CBaseEditorSession, CBaseEditorSession_slots, CModelDocEditorSession,
        CToolNodeTypeName,
    };

    use crate::generated::pins;
    use crate::log;
    use crate::modules;
    use crate::ops::ident::{mem_ok, obj_is};

    use super::{cstr, md_str, Chain, NodeTypes, NodeTypeRow, NAME_CAP};

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// C `g_modeldoc` (`mcp_bridge.c:5111`): the resolved app, pinned module -> stable
    /// identity. Main thread only (the tick's drain).
    static APP: Mutex<Option<usize>> = Mutex::new(None);

    /// C `g_mdCompile` + `g_mdCompileBound` (`mcp_bridge.c:5112-5113`): `Some` is bound.
    /// Main thread only.
    static MD_COMPILE: Mutex<Option<FnCompile>> = Mutex::new(None);

    /// The C's `ModelDocCompileFn` (`s2sdk.h:601`):
    /// `void (*)(CModelDocEditorSession *, int)`.
    type FnCompile = unsafe extern "C" fn(*mut c_void, i32);

    /// `modeldoc_bind` (`mcp_bridge.c:5116-5119`): bind the compile fn from the pinned
    /// `MDFN_SESSION_COMPILE` RVA after verification; a second call is a no-op. Main thread
    /// only, and only reached with `mod_ok(PIN_MODELDOC)` (`:5117`).
    fn modeldoc_bind() {
        if lock(&MD_COMPILE).is_some() {
            return;
        }
        if !modules::live().is_verified(pins::PIN_MODELDOC) {
            return;
        }
        let address = modules::live().base(pins::PIN_MODELDOC) as usize
            + pins::MDFN_SESSION_COMPILE.value as usize;
        // SAFETY: `address` is a verified module's image at the pinned function RVA (the module
        // gate re-checked the prologue bytes) and `FnCompile` is the C's own signature
        // (`s2sdk.h:601`). The C writes the same address into the same typed slot
        // (`mcp_bridge.c:5118`).
        unsafe { *lock(&MD_COMPILE) = Some(core::mem::transmute::<usize, FnCompile>(address)) };
    }

    /// `resolve_modeldoc` (`mcp_bridge.c:5124-5145`): the `CModelDocEditorApp *` through
    /// modeldoc_editor.dll's exported `CreateInterface("ToolSystem2_ModelDoc")`, every
    /// identity check and log line kept. Main thread only.
    fn resolve_modeldoc() -> Option<usize> {
        if let Some(app) = *lock(&APP) {
            return Some(app); // :5125
        }
        if !modules::live().is_verified(pins::PIN_MODELDOC) {
            log::line("modeldoc: module not verified"); // :5126
            return None;
        }
        let mb = modules::live().base(pins::PIN_MODELDOC) as usize;
        let name = pins::PIN_MODULES[usize::from(pins::PIN_MODELDOC)].name;
        // The image must be the verified one: the one game-module finder opens the game's own
        // `modeldoc_editor.dll`, and its base must be the base the gate verified - the C's
        // `GetModuleHandleA(name) != g_modBase[PIN_MODELDOC]` (`:5127-5129`), the same shape
        // `ops::hammer`'s `resolve_hammer` uses.
        let Some(module) = s2sdk::module::Module::find_game(name).ok().flatten() else {
            log::line("modeldoc: module handle changed"); // :5129
            return None;
        };
        if module.image().base() != mb as u64 {
            log::line("modeldoc: module handle changed"); // :5129
            return None;
        }
        let Some(sym) = module.symbol("CreateInterface") else {
            log::line("modeldoc: CreateInterface export missing"); // :5131
            return None;
        };
        // `S2CreateInterfaceFn` (`s2sdk.h:60`): called directly (not through
        // `Module::create_interface`) because the C's failure branch logs the factory's `ret`
        // (`:5135`), which the safe wrapper does not surface.
        type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
        // SAFETY: `sym` is the module's exported factory, found by name in the verified module.
        let factory: CreateInterfaceFn = unsafe { core::mem::transmute(sym) };
        let mut ret: c_int = -1;
        // SAFETY: the factory is a plain C call; the version string outlives the call and
        // `ret` is a valid out-parameter.
        let app = unsafe { factory(c"ToolSystem2_ModelDoc".as_ptr(), &mut ret) }; // :5133
        if app.is_null() || ret != 0 {
            // `lognum(.., (u64)(long long)ret)`: the sign-extended return code, unsigned
            // decimal (`mcp_bridge.c:5134-5135`).
            log::line(&format!(
                "modeldoc: ToolSystem2_ModelDoc unresolved, ret={}",
                (ret as i64) as u64
            ));
            return None;
        }
        if app as usize != mb + pins::MDG_APP_OBJ.value as usize {
            log::line("modeldoc: app identity mismatch"); // :5138
            return None;
        }
        if !obj_is(app, pins::PIN_MODELDOC, pins::MDV_APP_VT.value as u32) {
            log::line("modeldoc: app vtable mismatch"); // :5139
            return None;
        }
        modeldoc_bind(); // :5140
        *lock(&APP) = Some(app as usize); // :5141
        log::line("modeldoc: app resolved (ToolSystem2_ModelDoc export, ret 0)"); // :5142
        Some(app as usize)
    }

    /// One vtable slot's entry word of a session whose pinned vtable `obj_is` just proved.
    ///
    /// # Safety
    ///
    /// `session` passed `obj_is(MDV_SESSION_VT)`: its vptr (offset 0, the word `obj_is` itself
    /// read) is the pinned vtable, the vtable is readable, and `vt_slots_ok` proved every
    /// pinned slot - among them every slot this module calls - still holds its pinned target.
    unsafe fn session_slot(session: usize, slot: usize) -> usize {
        // SAFETY: the vptr word lies at offset 0 of the identity-checked object; `obj_is` read
        // the same word (`mcp_bridge.c:1638`).
        let vptr = unsafe { *(session as *const usize) };
        // SAFETY: slot `slot` of the pinned vtable - readable, and its target re-verified.
        unsafe { *((vptr + slot * core::mem::size_of::<usize>()) as *const usize) }
    }

    /// The production reader: one stateless object; its state is the module's pins and the
    /// process-wide app/compile statics.
    pub(super) struct LiveModelDoc;

    pub(super) static LIVE: LiveModelDoc = LiveModelDoc;

    impl Chain for LiveModelDoc {
        fn module_reason(&self) -> Option<&'static str> {
            if modules::live().is_verified(pins::PIN_MODELDOC) {
                return None;
            }
            Some(match modules::live().state(pins::PIN_MODELDOC) {
                Some(modules::State::Mismatch) => "modeldoc_mismatch",
                Some(modules::State::Verifying) => "modeldoc_verifying",
                _ => "modeldoc_not_loaded",
            })
        }

        fn app(&self) -> Option<usize> {
            resolve_modeldoc()
        }

        fn session(&self, app: usize) -> Option<usize> {
            // SAFETY: `app` is the pinned static `CModelDocEditorApp` (identity-checked by
            // `resolve_modeldoc`); `m_pActiveSession` is the generated `CBaseEditorApp` member
            // the C reads through `CModelDocEditorApp_ActiveSession` (`s2sdk.h:557`).
            let session = unsafe { (*(app as *const CBaseEditorApp)).m_pActiveSession as usize };
            (session != 0).then_some(session)
        }

        fn is_of(&self, object: usize, vt: u32) -> bool {
            obj_is(object as *const c_void, pins::PIN_MODELDOC, vt)
        }

        fn doc(&self, session: usize) -> usize {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)` in `active_modeldoc_with`;
            // `m_pDoc` is the generated member (`s2sdk.h:593`).
            let doc = unsafe { (*(session as *const CModelDocEditorSession)).m_pDoc };
            doc as usize
        }

        fn is_new(&self, session: usize) -> bool {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)`. The C reads
            // `session->isNewFile` (`mcp_bridge.c:5190`), the byte at the generated
            // `CBaseEditorSession::m_bIsNew`; only the byte's truthiness crosses, so a garbage
            // byte can never form an invalid Rust `bool`.
            unsafe {
                read_unaligned(
                    core::ptr::addr_of!(
                        (*(session as *const CModelDocEditorSession)).base.base.m_bIsNew
                    )
                    .cast::<u8>(),
                ) != 0
            }
        }

        fn has_asset(&self, session: usize) -> bool {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)`; `m_pAsset` is the generated
            // member (`s2sdk.h:583`).
            let asset = unsafe { (*(session as *const CModelDocEditorSession)).m_pAsset };
            !asset.is_null()
        }

        fn path(&self, session: usize, cap: usize) -> (i32, Vec<u8>) {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)`: its vptr is the pinned
            // `MDV_SESSION_VT` and slot 13 still holds its pinned target, so the typed field
            // is the game's GetPath (`s2sdk.h:569`).
            let get_path =
                unsafe { CBaseEditorSession::vtable(session as *const CBaseEditorSession).GetPath };
            let mut buf = vec![0u8; cap];
            let Some(get_path) = get_path else {
                return (0, Vec::new());
            };
            // SAFETY: the verified game's GetPath slot, called with the identity-checked
            // session; the answer is a game-owned C string (`s2sdk.h:569`).
            let p = unsafe { get_path(session as *mut CBaseEditorSession) };
            // SAFETY: `md_str` re-probes every page boundary.
            let status = unsafe { md_str(p.cast::<u8>(), &mut buf) };
            (status, cstr(&buf).to_vec())
        }

        fn is_modified(&self, session: usize) -> bool {
            // s2sdk.h:567 declares `u8 (*IsModified)(CModelDocEditorSession *self)`.
            // The generated `CBaseEditorSession_vtable::IsModified` returns a Rust `bool`,
            // which this port must never form from a game return (the wave-1 rule), so slot 11
            // is called through the C header's own u8 signature.
            type FnIsModified = unsafe extern "C" fn(*mut c_void) -> u8;
            // SAFETY: `active_modeldoc_with` proved the session's pinned vtable; slot 11 is
            // one of the pinned slots (`CBaseEditorSession_slots::IsModified`).
            let entry = unsafe { session_slot(session, CBaseEditorSession_slots::IsModified) };
            // SAFETY: the entry is the pinned target address of slot 11; the signature is the
            // C header's own.
            let is_modified: FnIsModified = unsafe { core::mem::transmute(entry) };
            // SAFETY: the verified game's IsModified slot, called with the identity-checked
            // session (`mcp_bridge.c:5184`).
            let raw = unsafe { is_modified(session as *mut c_void) };
            raw != 0
        }

        fn set_modified(&self, session: usize, modified: u8) {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)`; slot 12 is a pinned slot.
            let set_modified =
                unsafe { CBaseEditorSession::vtable(session as *const CBaseEditorSession).SetModified };
            if let Some(set_modified) = set_modified {
                // SAFETY: the verified game's SetModified slot, called with the
                // identity-checked session (`mcp_bridge.c:5325` passes 0).
                unsafe { set_modified(session as *mut CBaseEditorSession, modified != 0) };
            }
        }

        fn reload(&self, session: usize) {
            // SAFETY: the session passed `obj_is(MDV_SESSION_VT)`; slot 7 is a pinned slot.
            let reload =
                unsafe { CBaseEditorSession::vtable(session as *const CBaseEditorSession).ReloadDocument };
            if let Some(reload) = reload {
                // SAFETY: the verified game's ReloadDocument slot on the identity-checked
                // session (`mcp_bridge.c:5293`).
                unsafe { reload(session as *mut CBaseEditorSession) };
            }
        }

        fn save(&self, session: usize, old_path: &[u8], new_path: &[u8]) -> bool {
            // The C passes the same buffer twice (`mcp_bridge.c:5324`).
            let old = cstring(old_path);
            let new = cstring(new_path);
            // s2sdk.h:573 declares `u8 (*SaveAndNotify)(session, oldPath, newPath, u8,
            // u8)`; the generated field returns a Rust `bool`, so slot 18 is called through
            // the C header's own u8 signature (as `is_modified` above).
            type FnSaveAndNotify =
                unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u8, u8) -> u8;
            // SAFETY: `active_modeldoc_with` proved the session's pinned vtable; slot 18 is
            // one of the pinned slots (`CBaseEditorSession_slots::Save`).
            let entry = unsafe { session_slot(session, CBaseEditorSession_slots::Save) };
            // SAFETY: the entry is the pinned target address of slot 18; the signature is the
            // C header's own.
            let save: FnSaveAndNotify = unsafe { core::mem::transmute(entry) };
            // SAFETY: the verified game's SaveAndNotify slot, called with the identity-checked
            // session and the host-side path copies (`mcp_bridge.c:5324`, the FileSave call).
            let raw = unsafe { save(session as *mut c_void, old.as_ptr(), new.as_ptr(), 0, 0) };
            raw != 0
        }

        fn open(&self, app: usize, path: &[u8]) -> usize {
            let cpath = cstring(path);
            // SAFETY: `app` is the pinned static `CModelDocEditorApp` (identity-checked by
            // `resolve_modeldoc`); its vptr is the pinned `MDV_APP_VT` and slot 18 still holds
            // its pinned target, so the typed field is the game's OpenFile
            // (`s2sdk.h:551`).
            let open_file = unsafe { CBaseEditorApp::vtable(app as *const CBaseEditorApp).OpenFile };
            match open_file {
                // SAFETY: the verified game's OpenFile slot, called with the identity-checked
                // app and the host-side path buffer (`mcp_bridge.c:5259`, flags 1).
                Some(open_file) => unsafe {
                    open_file(app as *mut CBaseEditorApp, cpath.as_ptr(), 1) as usize
                },
                None => 0,
            }
        }

        fn compile(&self, session: usize) {
            modeldoc_bind(); // mcp_bridge.c:5343
            let Some(compile) = *lock(&MD_COMPILE) else {
                // Unreachable behind the method gate (`mod_ok(PIN_MODELDOC)` is checked first
                // and `modeldoc_bind` only refuses an unverified module): fail loudly instead
                // of inventing an answer.
                panic!("modeldoc: the compile fn never bound (modeldoc_editor.dll verified but MDFN_SESSION_COMPILE unreachable)");
            };
            // SAFETY: the pinned MDFN_SESSION_COMPILE function in the verified module, called
            // with the identity-checked session - the C's `g_mdCompile(session, 0)`
            // (`mcp_bridge.c:5344`, mode 0).
            unsafe { compile(session as *mut c_void, 0) };
        }

        fn node_types(&self) -> NodeTypes {
            let mb = modules::live().base(pins::PIN_MODELDOC) as usize;
            // SAFETY: `MDG_NODETYPE_COUNT` is a pinned `int` global in the verified module
            // (the method gate proved `mod_ok`); the C reads the same word
            // (`mcp_bridge.c:5354`).
            let count = unsafe {
                read_unaligned((mb + pins::MDG_NODETYPE_COUNT.value as usize) as *const i32)
            } as i64;
            // SAFETY: `MDG_NODETYPE_ARRAY` is a pinned pointer global in the verified module
            // (`mcp_bridge.c:5355`).
            let descs = unsafe {
                read_unaligned((mb + pins::MDG_NODETYPE_ARRAY.value as usize) as *const usize)
            };
            let slot_size = core::mem::size_of::<*const c_void>();
            if !(1..=4096).contains(&count)
                || descs == 0
                || !mem_ok(descs as *const c_void)
                || !mem_ok((descs + (count as usize - 1) * slot_size) as *const c_void)
            {
                return NodeTypes::Unusable(if count < 0 { 0 } else { count }); // :5356-5362
            }
            let name_off = core::mem::offset_of!(CToolNodeTypeName, m_pszName);
            let mut rows: Vec<NodeTypeRow> = Vec::new();
            for i in 0..count as usize {
                // The C reads `descs[i]` unguarded (`:5368`) after proving the array's first
                // and last slot readable; probing each slot first is the memory-safety fix
                // that changes no byte for a well-formed array (a corrupt middle slot answers
                // the same "node-type descriptor unreadable" error instead of faulting).
                let slot = descs + i * slot_size;
                if !mem_ok(slot as *const c_void) {
                    return NodeTypes::BadDescriptor;
                }
                // SAFETY: the slot word was just proven readable.
                let desc = unsafe { read_unaligned(slot as *const usize) };
                if desc == 0
                    || !mem_ok(desc as *const c_void)
                    || !mem_ok((desc + name_off + 7) as *const c_void)
                {
                    return NodeTypes::BadDescriptor; // :5370-5372
                }
                // SAFETY: the descriptor's first four bytes were just proven readable; the C
                // reads the same u32 (`mcp_bridge.c:5374`).
                let type_id = unsafe { read_unaligned(desc as *const u32) };
                // SAFETY: the pointer member at `name_off` was proven readable to its last
                // byte (`name_off + 7`, `mcp_bridge.c:5369`).
                let name_ptr = unsafe { read_unaligned((desc + name_off) as *const *const u8) };
                let mut nm = [0u8; NAME_CAP];
                // SAFETY: `md_str` re-probes every page boundary.
                if unsafe { md_str(name_ptr, &mut nm) } != 1 {
                    return NodeTypes::BadName; // :5376-5378
                }
                rows.push((type_id, cstr(&nm).to_vec()));
            }
            NodeTypes::Rows(rows)
        }
    }

    /// A host-side path as a `CString` for a vtable call.
    fn cstring(bytes: &[u8]) -> std::ffi::CString {
        std::ffi::CString::new(cstr(bytes).to_vec())
            .expect("a C string from json_string/md_str has no interior NUL")
    }
}

/// The production reader on the Windows tools build.
#[cfg(windows)]
pub(crate) fn game() -> &'static dyn Chain {
    &win::LIVE
}

/// Non-Windows: there is no modeldoc_editor.dll (the tools DLL only ever runs on Windows), so
/// the method gate answers `-32017` before a body starts. A body that still runs (an injected
/// ported set in a test) fails loudly where the C would read its module, and answers the C's
/// own module-state reasons where the C answers them.
#[cfg(not(windows))]
pub(crate) fn game() -> &'static dyn Chain {
    &NoModelDoc
}

/// The absent-platform reader.
#[cfg(not(windows))]
struct NoModelDoc;

#[cfg(not(windows))]
impl Chain for NoModelDoc {
    fn module_reason(&self) -> Option<&'static str> {
        Some("modeldoc_not_loaded")
    }
    fn app(&self) -> Option<usize> {
        None
    }
    fn session(&self, _app: usize) -> Option<usize> {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn is_of(&self, _object: usize, _vt: u32) -> bool {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn doc(&self, _session: usize) -> usize {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn is_new(&self, _session: usize) -> bool {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn has_asset(&self, _session: usize) -> bool {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn path(&self, _session: usize, _cap: usize) -> (i32, Vec<u8>) {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn is_modified(&self, _session: usize) -> bool {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn set_modified(&self, _session: usize, _modified: u8) {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn reload(&self, _session: usize) {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn save(&self, _session: usize, _old_path: &[u8], _new_path: &[u8]) -> bool {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn open(&self, _app: usize, _path: &[u8]) -> usize {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn compile(&self, _session: usize) {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
    fn node_types(&self) -> NodeTypes {
        panic!("modeldoc: no modeldoc_editor.dll on this platform");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

    /// A fake reader: canned chain state, a call log, and every mock value citing the C line
    /// or header line it mirrors. The mocks assert the arguments they receive.
    struct Fake {
        /// `module_reason`'s answer (`None` = the module is verified).
        reason: Option<&'static str>,
        /// `resolve_modeldoc`'s answer.
        app: Cell<Option<usize>>,
        /// `ActiveSession(app)` (`s2sdk.h:557`).
        session: Cell<Option<usize>>,
        /// `obj_is(session, MDV_SESSION_VT)`.
        session_ok: bool,
        /// `session->doc` (`s2sdk.h:593`).
        doc: usize,
        /// `obj_is(doc, MDV_DOC_VT)`.
        doc_ok: bool,
        /// `session->isNewFile` (`s2sdk.h:579`).
        is_new: bool,
        /// `session->asset` (`s2sdk.h:583`).
        asset: usize,
        /// `md_str(GetPath(session))` (`s2sdk.h:569`).
        path: (i32, Vec<u8>),
        /// `IsModified(session)` (`s2sdk.h:567`).
        dirty: bool,
        /// `OpenFile(app, path, 1)`'s answer (`s2sdk.h:551`).
        open_result: usize,
        /// `SaveAndNotify(...)`'s answer (`s2sdk.h:573`).
        save_result: bool,
        /// `nodeTypes`' table read.
        node_types: Option<NodeTypes>,
        /// `reload()` drops the session before the re-resolution (the C's "reload left no
        /// valid document" path, `mcp_bridge.c:5293-5297`).
        session_gone_after_reload: bool,

        app_calls: Cell<u32>,
        open_args: RefCell<Vec<(usize, Vec<u8>)>>,
        reload_calls: Cell<u32>,
        save_calls: Cell<u32>,
        save_args: RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
        set_modified_calls: Cell<u32>,
        compile_calls: Cell<u32>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                reason: None,
                app: Cell::new(Some(0x4000_0000)),
                session: Cell::new(Some(0x5000_0000)),
                session_ok: true,
                doc: 0x6000_0000,
                doc_ok: true,
                is_new: false,
                asset: 0x10,
                path: (1, b"C:\\games\\models\\kit\\kit.vmdl".to_vec()),
                dirty: false,
                open_result: 0,
                save_result: true,
                node_types: None,
                session_gone_after_reload: false,
                app_calls: Cell::new(0),
                open_args: RefCell::new(Vec::new()),
                reload_calls: Cell::new(0),
                save_calls: Cell::new(0),
                save_args: RefCell::new(Vec::new()),
                set_modified_calls: Cell::new(0),
                compile_calls: Cell::new(0),
            }
        }
    }

    impl Chain for Fake {
        fn module_reason(&self) -> Option<&'static str> {
            self.reason
        }
        fn app(&self) -> Option<usize> {
            self.app_calls.set(self.app_calls.get() + 1);
            self.app.get()
        }
        fn session(&self, _app: usize) -> Option<usize> {
            self.session.get()
        }
        fn is_of(&self, object: usize, vt: u32) -> bool {
            if Some(object) == self.session.get() {
                vt == pins::MDV_SESSION_VT.value as u32 && self.session_ok
            } else {
                vt == pins::MDV_DOC_VT.value as u32 && self.doc_ok
            }
        }
        fn doc(&self, _session: usize) -> usize {
            self.doc
        }
        fn is_new(&self, _session: usize) -> bool {
            self.is_new
        }
        fn has_asset(&self, _session: usize) -> bool {
            self.asset != 0
        }
        fn path(&self, _session: usize, cap: usize) -> (i32, Vec<u8>) {
            assert_eq!(cap, PATH_CAP, "the C's path buffers are 1024 bytes");
            let mut bytes = self.path.1.clone();
            bytes.resize(cap, 0);
            (self.path.0, cstr(&bytes).to_vec())
        }
        fn is_modified(&self, _session: usize) -> bool {
            self.dirty
        }
        fn set_modified(&self, _session: usize, modified: u8) {
            assert_eq!(modified, 0, "mcp_bridge.c:5325: FileSave passes 0");
            self.set_modified_calls.set(self.set_modified_calls.get() + 1);
        }
        fn reload(&self, _session: usize) {
            self.reload_calls.set(self.reload_calls.get() + 1);
            if self.session_gone_after_reload {
                self.session.set(None);
            }
        }
        fn save(&self, _session: usize, old_path: &[u8], new_path: &[u8]) -> bool {
            // s2sdk.h:573: `SaveAndNotify(session, oldPath, newPath, u8 a4, u8 a5)`;
            // the C passes the same buffer twice (mcp_bridge.c:5324).
            self.save_calls.set(self.save_calls.get() + 1);
            self.save_args.borrow_mut().push((old_path.to_vec(), new_path.to_vec()));
            self.save_result
        }
        fn open(&self, app: usize, path: &[u8]) -> usize {
            // s2sdk.h:551: `OpenFile(app, const char *path, u32 flags)`; the C passes
            // flags 1 (mcp_bridge.c:5259) - the flag lives in the reader, not the op.
            self.open_args.borrow_mut().push((app, path.to_vec()));
            self.open_result
        }
        fn compile(&self, _session: usize) {
            self.compile_calls.set(self.compile_calls.get() + 1);
        }
        fn node_types(&self) -> NodeTypes {
            self.node_types
                .clone()
                .expect("the test must install a nodeTypes answer")
        }
    }

    fn out_buf() -> Vec<u8> {
        vec![0u8; RESULT_BUF]
    }

    fn sb_str(out: &Sb) -> String {
        String::from_utf8_lossy(out.as_cstr()).into_owned()
    }

    // -----------------------------------------------------------------------------------------
    // md_str (`mcp_bridge.c:5169-5180`)
    // -----------------------------------------------------------------------------------------

    /// `md_str`'s three answers over host memory (its game-pointer safety is the same
    /// `mem_ok`-probed walk `hammer.rs`'s `probe_str` tests).
    #[test]
    fn md_str_copies_cuts_and_refuses() {
        let mut out = [0u8; 8];
        // SAFETY: a host buffer holding a NUL-terminated C string.
        assert_eq!(unsafe { md_str(c"abc".as_ptr().cast(), &mut out) }, 1);
        assert_eq!(cstr(&out), b"abc");
        // SAFETY: a host buffer; `kit.vmdl` fills cap-1 bytes with no NUL (a cut, `:5178`).
        assert_eq!(
            unsafe { md_str(b"kit.vmdl".as_ptr(), &mut out) },
            -1,
            "no NUL inside cap-1: cut"
        );
        assert_eq!(cstr(&out), b"kit.vmd"); // cut at cap-1, the C's `out[i] = 0` (`:5178`)
        // SAFETY: the C answers 0 for a null pointer (`:5172`).
        assert_eq!(unsafe { md_str(core::ptr::null(), &mut out) }, 0);
        assert_eq!(out[0], 0);
    }

    // -----------------------------------------------------------------------------------------
    // `md_doc_json` (`mcp_bridge.c:5182-5194`)
    // -----------------------------------------------------------------------------------------

    /// The doc object, byte for byte: the expected string is read off the C's `sb_add`
    /// sequence (`:5188-5193`).
    #[test]
    fn md_doc_json_is_byte_exact() {
        let facts = MdFacts {
            path: b"C:\\games\\models\\kit\\kit.vmdl".to_vec(),
            dirty: true,
            is_new: false,
            has_asset: true,
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        md_doc_json_into(&mut out, &facts);
        assert_eq!(
            sb_str(&out),
            "{\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\"dirty\":true,\
             \"isNewFile\":false,\"hasAsset\":true,\"compileStatus\":null,\"docType\":\"vmdl\"}"
        );
        // The C hands the raw bytes to `sb_addjson` (`:5188`): invalid UTF-8 becomes U+FFFD
        // there, and a quote is escaped - never the reader's business.
        let facts = MdFacts {
            path: vec![b'a', 0xFF, b'"', b'b'],
            dirty: false,
            is_new: true,
            has_asset: false,
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        md_doc_json_into(&mut out, &facts);
        assert_eq!(
            sb_str(&out),
            "{\"path\":\"a\u{FFFD}\\\"b\",\"dirty\":false,\"isNewFile\":true,\
             \"hasAsset\":false,\"compileStatus\":null,\"docType\":\"vmdl\"}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `active_modeldoc`'s reason chain (`mcp_bridge.c:5147-5167`)
    // -----------------------------------------------------------------------------------------

    /// Every hop's refusal reason, in the C's order.
    #[test]
    fn active_modeldoc_walks_the_c_reason_chain() {
        for reason in ["modeldoc_not_loaded", "modeldoc_mismatch", "modeldoc_verifying"] {
            let md = Fake {
                reason: Some(reason),
                ..Fake::default()
            };
            assert_eq!(active_modeldoc_with(&md), Err(reason), "`:5149-5154`");
        }
        let md = Fake {
            app: Cell::new(None),
            ..Fake::default()
        };
        assert_eq!(active_modeldoc_with(&md), Err("modeldoc_unavailable"));
        let md = Fake {
            session: Cell::new(None),
            ..Fake::default()
        };
        assert_eq!(active_modeldoc_with(&md), Err("no_session"));
        let md = Fake {
            session_ok: false,
            ..Fake::default()
        };
        assert_eq!(active_modeldoc_with(&md), Err("bad_session"));
        let md = Fake {
            doc: 0,
            ..Fake::default()
        };
        assert_eq!(active_modeldoc_with(&md), Err("no_document"));
        let md = Fake {
            doc_ok: false,
            ..Fake::default()
        };
        assert_eq!(active_modeldoc_with(&md), Err("bad_doc"));
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.active` (`mcp_bridge.c:5201-5213`)
    // -----------------------------------------------------------------------------------------

    /// The no-doc shape (`{"doc":null,"reason":..}`) and the doc shape, byte for byte.
    #[test]
    fn active_answers_both_shapes_without_erroring() {
        let md = Fake {
            session: Cell::new(None),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_active_with(&md, &mut out).expect("active never errors");
        assert_eq!(sb_str(&out), "{\"doc\":null,\"reason\":\"no_session\"}");

        let md = Fake::default();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_active_with(&md, &mut out).expect("active never errors");
        assert_eq!(
            sb_str(&out),
            "{\"doc\":{\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\"dirty\":false,\
             \"isNewFile\":false,\"hasAsset\":true,\"compileStatus\":null,\"docType\":\"vmdl\"}}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.open` (`mcp_bridge.c:5214-5272`)
    // -----------------------------------------------------------------------------------------

    fn open_err(params: Option<&str>, fake: &Fake) -> OpErr {
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_open_with(fake, params, &mut out).expect_err("the open arm must refuse")
    }

    /// Every malformed-params path answers the C's code, name and `reason` - and none of them
    /// touches the game (the C validates first, `mcp_bridge.c:5217-5256`).
    #[test]
    fn open_validates_params_before_touching_the_game() {
        let fake = Fake::default();
        for params in [None, Some("{}"), Some(r#"{"path":""}"#), Some(r#"{"path":123}"#)] {
            let err = open_err(params, &fake);
            assert_eq!(err.code, -32602, "{params:?}");
            assert_eq!(err.name, "InvalidParams", "{params:?}");
        }
        assert_eq!(
            open_err(Some(r#"{"nope":1}"#), &fake).data,
            r#"{"reason":"path: relative to the addon content root (models/...), 1..512 chars, no '..'"}"#,
            "`:5218`: the first refusal's exact bytes"
        );
        assert_eq!(
            open_err(
                Some(&format!(r#"{{"path":"{}"}}"#, "a".repeat(513))),
                &fake
            )
            .data,
            r#"{"reason":"path: 1..512 chars"}"#,
            "`:5222`: 513 collapsed chars"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"models\\kit.vmdl"}"#), &fake).data,
            r#"{"reason":"path: no JSON escapes (use '/' separators)"}"#,
            "`:5229`: one backslash in the raw JSON text"
        );
        assert_eq!(
            open_err(Some("{\"path\":\"a\u{1}b.vmdl\"}"), &fake).data,
            r#"{"reason":"path: no control characters"}"#,
            "`:5235`: a raw control byte"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"/models/kit.vmdl"}"#), &fake).data,
            r#"{"reason":"path: relative to the addon content root (models/...), not absolute"}"#,
            "`:5240`: leading slash"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"a:kit.vmdl"}"#), &fake).data,
            r#"{"reason":"path: relative to the addon content root (models/...), not absolute"}"#,
            "`:5240`: a drive-letter colon at index 1"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"models/kit.vmap"}"#), &fake).data,
            r#"{"reason":"path: a .vmdl file"}"#,
            "`:5244`: the extension"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"models/kit.vmdl2"}"#), &fake).data,
            r#"{"reason":"path: a .vmdl file"}"#,
            "`:5244`: a .vmdl prefix is not the extension"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"models/../kit.vmdl"}"#), &fake).data,
            r#"{"reason":"path: no '..'"}"#,
            "`:5253`"
        );
        assert_eq!(
            open_err(Some(r#"{"path":"models/a..b.vmdl"}"#), &fake).data,
            r#"{"reason":"path: no '..'"}"#,
            "`:5253`: any dot pair, not only a leading one"
        );
        assert_eq!(
            fake.app_calls.get(),
            0,
            "every refusal above happened before resolve_modeldoc (`:5257`)"
        );
    }

    /// `json_string` is depth-blind, exactly as the C's: a nested `path` member is found and
    /// used (`mcp_bridge.c:5217`, `json_find` at `:328`).
    #[test]
    fn open_finds_the_path_depth_blind() {
        let fake = Fake {
            open_result: 0x5000_0000,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_open_with(
            &fake,
            Some(r#"{"nested":{"path":"models/kit.vmdl"}}"#),
            &mut out,
        )
        .expect("the depth-blind finder finds the nested member");
        assert_eq!(*fake.open_args.borrow(), vec![(0x4000_0000, b"models/kit.vmdl".to_vec())]);
    }

    /// A valid path reaches `OpenFile` with the collapsed path, and the reply carries both the
    /// opened flag and the re-resolved active document (`mcp_bridge.c:5257-5271`).
    #[test]
    fn open_calls_openfile_and_reports_the_active_document() {
        let fake = Fake {
            open_result: 0x5000_0000,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_open_with(&fake, Some(r#"{"path":"models/kit.vmdl"}"#), &mut out)
            .expect("a valid open");
        assert_eq!(
            *fake.open_args.borrow(),
            vec![(0x4000_0000, b"models/kit.vmdl".to_vec())],
            "OpenFile(app, the collapsed path) - flags 1 lives in the reader (`:5259`)"
        );
        assert_eq!(
            sb_str(&out),
            "{\"opened\":true,\"path\":\"models/kit.vmdl\",\"active\":\
             {\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\"dirty\":false,\
             \"isNewFile\":false,\"hasAsset\":true,\"compileStatus\":null,\"docType\":\"vmdl\"}}"
        );
        // A refused OpenFile still answers the active (prior) document (`:5263-5270`).
        let fake = Fake::default();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_open_with(&fake, Some(r#"{"path":"models/kit.vmdl"}"#), &mut out)
            .expect("the arm answers, opened:false");
        assert_eq!(
            sb_str(&out),
            "{\"opened\":false,\"path\":\"models/kit.vmdl\",\"active\":\
             {\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\"dirty\":false,\
             \"isNewFile\":false,\"hasAsset\":true,\"compileStatus\":null,\"docType\":\"vmdl\"}}"
        );
        // An unresolved app answers the C's `-32003` (`:5258`).
        let fake = Fake {
            app: Cell::new(None),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_open_with(&fake, Some(r#"{"path":"models/kit.vmdl"}"#), &mut out)
            .expect_err("no app, no open");
        assert_eq!(
            err,
            OpErr::new(-32003, "NoActiveDocument", "modeldoc_unavailable")
        );
        // The re-resolved active document can be gone after the open (`:5269`).
        let fake = Fake {
            open_result: 0x5000_0000,
            session: Cell::new(None),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_open_with(&fake, Some(r#"{"path":"models/kit.vmdl"}"#), &mut out)
            .expect("the arm answers");
        assert_eq!(
            sb_str(&out),
            "{\"opened\":true,\"path\":\"models/kit.vmdl\",\"active\":null}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.reload` (`mcp_bridge.c:5273-5305`)
    // -----------------------------------------------------------------------------------------

    /// A dirty document refuses without `force`, reloads with it (`mcp_bridge.c:5283-5285`).
    #[test]
    fn reload_refuses_a_dirty_document_without_force() {
        let fake = Fake {
            dirty: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_reload_with(&fake, None, &mut out).expect_err("dirty, no force");
        assert_eq!(err.code, -32016);
        assert_eq!(err.name, "UnsavedChanges");
        assert_eq!(
            err.data,
            r#"{"reason":"document is dirty; pass force:true to discard"}"#
        );
        assert_eq!(fake.reload_calls.get(), 0, "the refusal precedes the reload");
        // `force` is the value's first byte (`mcp_bridge.c:5280-5281`).
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_reload_with(&fake, Some(r#"{"force":true}"#), &mut out).expect("force:true reloads");
        assert_eq!(fake.reload_calls.get(), 1);
        // Any other first byte is not `t` (the C's `f[0] == 't'`).
        let fake = Fake {
            dirty: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_reload_with(&fake, Some(r#"{"force":false}"#), &mut out)
            .expect_err("force:false is not force");
        assert_eq!(fake.reload_calls.get(), 0);
    }

    /// An untitled document refuses before the reload (`mcp_bridge.c:5289-5292`), and a clean
    /// one reloads and reports the captured path (`:5293-5303`).
    #[test]
    fn reload_reports_the_captured_path_and_the_reload() {
        let fake = Fake {
            is_new: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_reload_with(&fake, None, &mut out).expect_err("untitled");
        assert_eq!(
            err,
            OpErr::new(-32015, "UntitledDocument", "no path to reload from")
        );
        assert_eq!(fake.reload_calls.get(), 0);

        let fake = Fake {
            path: (1, Vec::new()),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_reload_with(&fake, None, &mut out).expect_err("an empty GetPath");
        assert_eq!(
            err,
            OpErr::new(-32015, "UntitledDocument", "no path to reload from")
        );

        let fake = Fake::default();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_reload_with(&fake, None, &mut out).expect("a clean reload");
        assert_eq!(fake.reload_calls.get(), 1);
        assert_eq!(
            sb_str(&out),
            "{\"reloaded\":true,\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\
             \"discardedChanges\":false}"
        );

        // A reload that left no valid document still answers - with reloaded:false
        // (`mcp_bridge.c:5293-5303`): the session is dropped and the re-resolution fails.
        let fake = Fake {
            session_gone_after_reload: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_reload_with(&fake, None, &mut out).expect("the arm answers reloaded:false");
        assert_eq!(fake.reload_calls.get(), 1, "the reload itself ran");
        assert_eq!(
            sb_str(&out),
            "{\"reloaded\":false,\"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\",\
             \"discardedChanges\":false}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.save` (`mcp_bridge.c:5306-5333`)
    // -----------------------------------------------------------------------------------------

    /// An untitled document refuses (`mcp_bridge.c:5310-5313`), as does a path that did not
    /// copy whole (`:5316-5319`).
    #[test]
    fn save_refuses_untitled_and_unusable_paths() {
        let fake = Fake {
            is_new: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_save_with(&fake, &mut out).expect_err("untitled");
        assert_eq!(
            err,
            OpErr::new(
                -32015,
                "UntitledDocument",
                "no path (Save As would open a modal)"
            )
        );
        assert_eq!(fake.save_calls.get(), 0);

        // `md_str` cut the path (status -1): the strict check refuses (`mcp_bridge.c:5316`).
        let fake = Fake {
            path: (-1, b"C:\\games\\models\\kit\\kit.vmd".to_vec()),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_save_with(&fake, &mut out).expect_err("a cut path");
        assert_eq!(
            err,
            OpErr::new(
                -32015,
                "UntitledDocument",
                "no usable path (Save As would open a modal)"
            )
        );
        assert_eq!(fake.save_calls.get(), 0);

        // An unreadable path string answers 0 from md_str (`mcp_bridge.c:5172`).
        let fake = Fake {
            path: (0, Vec::new()),
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_save_with(&fake, &mut out).expect_err("an unreadable path");
        assert_eq!(
            err,
            OpErr::new(
                -32015,
                "UntitledDocument",
                "no usable path (Save As would open a modal)"
            )
        );
    }

    /// A clean document skips the save (`skippedClean`, no SaveAndNotify call); a dirty one
    /// calls `SaveAndNotify(path, path, 0, 0)` and `SetModified(0)` on success
    /// (`mcp_bridge.c:5321-5327`).
    #[test]
    fn save_calls_save_and_notify_with_the_path_twice() {
        let fake = Fake::default();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_save_with(&fake, &mut out).expect("a clean save");
        assert_eq!(fake.save_calls.get(), 0, "clean documents skip the save call");
        assert_eq!(fake.set_modified_calls.get(), 0);
        assert_eq!(
            sb_str(&out),
            "{\"saved\":true,\"skippedClean\":true,\
             \"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\"}"
        );

        let fake = Fake {
            dirty: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_save_with(&fake, &mut out).expect("a dirty save");
        assert_eq!(fake.save_calls.get(), 1);
        assert_eq!(
            *fake.save_args.borrow(),
            vec![(fake.path.1.clone(), fake.path.1.clone())],
            "SaveAndNotify(session, path, path, 0, 0) - the C's FileSave call (`:5324`)"
        );
        assert_eq!(fake.set_modified_calls.get(), 1, "`:5325`: mirrors FileSave");
        assert_eq!(
            sb_str(&out),
            "{\"saved\":true,\"skippedClean\":false,\
             \"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\"}"
        );

        // A failed save does not clear the modified flag (`mcp_bridge.c:5325`).
        let fake = Fake {
            dirty: true,
            save_result: false,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_save_with(&fake, &mut out).expect("the arm answers saved:false");
        assert_eq!(fake.set_modified_calls.get(), 0);
        assert_eq!(
            sb_str(&out),
            "{\"saved\":false,\"skippedClean\":false,\
             \"path\":\"C:\\\\games\\\\models\\\\kit\\\\kit.vmdl\"}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.compile` (`mcp_bridge.c:5334-5351`)
    // -----------------------------------------------------------------------------------------

    /// A session without an asset refuses before the compile call (`mcp_bridge.c:5338-5341`).
    #[test]
    fn compile_refuses_without_an_asset() {
        let fake = Fake {
            asset: 0,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        let err = op_compile_with(&fake, &mut out).expect_err("no asset");
        assert_eq!(err, OpErr::new(-32017, "PrerequisiteFailed", "no_asset"));
        assert_eq!(fake.compile_calls.get(), 0, "the refusal precedes the call");
    }

    /// The compile call runs once and the answer is honest about the unknown outcome
    /// (`mcp_bridge.c:5343-5349`).
    #[test]
    fn compile_calls_the_bound_fn_and_reports_saved_first() {
        let fake = Fake {
            dirty: true,
            ..Fake::default()
        };
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_compile_with(&fake, &mut out).expect("a compile");
        assert_eq!(fake.compile_calls.get(), 1);
        assert_eq!(
            sb_str(&out),
            "{\"status\":null,\"failed\":null,\"savedFirst\":true}"
        );

        let fake = Fake::default();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        op_compile_with(&fake, &mut out).expect("a compile of a clean document");
        assert_eq!(
            sb_str(&out),
            "{\"status\":null,\"failed\":null,\"savedFirst\":false}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // `modeldoc.nodeTypes` (`mcp_bridge.c:5352-5387`)
    // -----------------------------------------------------------------------------------------

    /// The row builder, byte for byte: the expected string is read off the C's `sb_add`
    /// sequence (`mcp_bridge.c:5364-5385`).
    #[test]
    fn node_types_rows_are_byte_exact() {
        let rows: Vec<NodeTypeRow> = vec![
            (1, b"World".to_vec()),
            (2, b"Mesh".to_vec()),
            (0xFFFFFFFF, vec![b'x', 0xFF, b'y']),
        ];
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        node_types_into(&mut out, &rows);
        assert_eq!(
            sb_str(&out),
            "{\"count\":3,\"types\":[{\"typeId\":1,\"className\":\"World\"},\
             {\"typeId\":2,\"className\":\"Mesh\"},\
             {\"typeId\":4294967295,\"className\":\"x\u{FFFD}y\"}]}"
        );
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        node_types_into(&mut out, &[]);
        assert_eq!(sb_str(&out), "{\"count\":0,\"types\":[]}");
    }

    /// Every refusal of the table walk, with the C's exact reasons
    /// (`mcp_bridge.c:5356-5362`, `:5370-5378`).
    #[test]
    fn node_types_refusals_carry_the_c_reasons() {
        for (count, printed) in [
            (0, "0"),
            (-5, "0"),
            (5000, "5000"),
            (-1, "0"),
        ] {
            let fake = Fake {
                node_types: Some(NodeTypes::Unusable(count)),
                ..Fake::default()
            };
            let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
            let err = op_node_types_with(&fake, &mut out).expect_err("an unusable table");
            assert_eq!(err.code, -32017);
            assert_eq!(err.name, "PrerequisiteFailed");
            assert_eq!(
                err.data,
                format!(r#"{{"reason":"node-type table unusable (count {printed})"}}"#),
                "`:5359`: the C prints `count < 0 ? 0 : count`"
            );
        }
        for (bad, reason) in [
            (NodeTypes::BadDescriptor, "node-type descriptor unreadable"),
            (NodeTypes::BadName, "node-type className unreadable"),
        ] {
            let fake = Fake {
                node_types: Some(bad),
                ..Fake::default()
            };
            let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
            let err = op_node_types_with(&fake, &mut out).expect_err("an unreadable walk");
            assert_eq!(err.code, -32017);
            assert_eq!(err.name, "PrerequisiteFailed");
            assert_eq!(err.data, format!(r#"{{"reason":"{reason}"}}"#));
        }
    }

    /// `nodeTypes`' overflow: 4096 wide rows overflow the arm's 256 KiB result buffer and the
    /// arm answers the C's `-32603 "InternalError: modeldoc result too large"`
    /// (`mcp_bridge.c:6206`, `:6211-6212`).
    #[test]
    fn node_types_overflows_at_the_c_256k() {
        let rows: Vec<NodeTypeRow> = (0..4096).map(|i| (i as u32, vec![b'x'; 96])).collect();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        node_types_into(&mut out, &rows);
        assert!(
            out.ovf(),
            "4096 x ~110 bytes must overflow the C's 256 KiB result buffer"
        );
        let reply = finish("g1", Ok(()), &out, MODELDOC_OVERFLOW);
        assert_eq!(
            reply,
            crate::control::error(Some("g1"), -32603, MODELDOC_OVERFLOW)
        );
        // And a comfortable fit does not: 4096 minimal rows stay under the cap.
        let rows: Vec<NodeTypeRow> = (0..4096).map(|i| (i as u32, b"M".to_vec())).collect();
        let mut buf = out_buf();
        let mut out = Sb::new(&mut buf);
        node_types_into(&mut out, &rows);
        assert!(!out.ovf(), "4096 x ~40 bytes fit the C's 256 KiB buffer");
    }

    // -----------------------------------------------------------------------------------------
    // The handlers' envelope (the C's dispatch arm, `mcp_bridge.c:6205-6219`)
    // -----------------------------------------------------------------------------------------

    /// A handler's reply frame is the C's: the result envelope around the arm's result bytes.
    #[test]
    fn a_handler_wraps_the_result_in_the_c_envelope() {
        let req = Request {
            id: "g1".to_string(),
            method: "modeldoc.active".to_string(),
            params: None,
            received: std::time::Instant::now(),
            deadline: None,
            gen: 1,
        };
        // The host reader answers the C's no-module reason (`modeldoc_not_loaded`).
        let reply = modeldoc_active(&req);
        assert_eq!(
            reply,
            crate::control::result_ok(
                Some("g1"),
                "{\"doc\":null,\"reason\":\"modeldoc_not_loaded\"}"
            )
        );
    }
}
