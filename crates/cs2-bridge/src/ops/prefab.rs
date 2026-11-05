//! `node.createPrefab` and `node.prefabFromSelection` (bridge m3.18): Hammer's two non-UI prefab
//! workers, driven the way the editor's own commands drive them.
//!
//! RE notes: `target/orch/import/re-prefab.md` (hammer.dll 2000933). Both calls follow the
//! command handlers the notes read:
//!
//! * `node.createPrefab` is the "ImportPrefab" command without its file dialog:
//!   `AddOperation(hist, selection, label, 0)` -> `HFN_DOC_ADD_PREFAB(doc, abs)` ->
//!   `RecordNode(hist, prefab + IUndoable, 0)`, then the existing origin/angles writes.
//! * `node.prefabFromSelection` is "CreatePrefabFromSelection(Centered)" without its save dialog:
//!   `AddOperation(...)` -> `HFN_DOC_PREFAB_FROM_SELECTION(doc, abs, frame, centered)`. The worker
//!   copies the selection into a new prefab, removes the originals, **writes the new .vmap
//!   immediately** and selects the prefab.
//!
//! Every path is resolved exactly like `map.open`'s: the addon-relative `maps/...vmap` shape gate,
//! the absolute content path from the process facts, never anything outside the addon.
//!
//! # What is shared
//!
//! The group is a small trait ([`Prefab`]) so the sequences are testable without Hammer; the live
//! implementation ([`GamePrefab`]) forwards the generic document/undo/transform steps to
//! `ops::hammer`'s live writer (`undo_open`, `set_origin`, `set_angles`, `doc_set_modified`), the
//! same calls `entity.create` and `node.setTransform` make, and binds the five new pins.

use core::ffi::CStr;
use std::ffi::CString;

use crate::entity_json::{json_find_top, json_float3_top, xform_origin_ok, xform_origin_reason, Sb};
use crate::ops::hammer::{addf3, Doc, Node};
use crate::ops::map::{self, HostFact};
use crate::ops::OpErr;
use crate::queue::Request;

/// The undo label of `node.createPrefab`.
const CREATE_LABEL: &CStr = c"MCP: create prefab";
/// The undo label of `node.prefabFromSelection`.
const FROM_SELECTION_LABEL: &CStr = c"MCP: create prefab from selection";
/// The reply buffer: a path, three vectors and a few words.
const REPLY_CAP: usize = 8192;
/// The longest `targetMapPath` read back from the game.
#[cfg_attr(not(windows), allow(dead_code))]
const TARGET_CAP: usize = 1024;

/// The work-frame struct `HFN_DOC_PREFAB_FROM_SELECTION` takes (`CMapDocSession+0xDA8` in the UI
/// path, a `0xB4`-byte struct; RE notes section 3): the worker only reads a 3x4 rotation matrix
/// at `+0x24` of it, to turn into the prefab's angles. The notes recommend a zero-filled buffer
/// of at least `0xB4` bytes instead of reproducing the layout: `MatrixAngles` of an all-zero
/// matrix yields angles `(0, 0, 0)`, the same as an identity frame, and a zero buffer cannot be
/// wrong about an offset the headers do not declare. 256 bytes, as the notes use.
pub(crate) const FRAME_LEN: usize = 256;

/// The selection modes the worker accepts: groups, objects, meshes (RE notes section 3:
/// "selection mode must be groups, objects, or meshes").
const MODE_MAX: i32 = 2;

/// What the selection looks like, as the pre-checks need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelState {
    /// The `k` in `0..=7` with `GET_SET(sel, k) == CURRENT_SET(sel)`; `None` if none matches.
    pub mode: Option<i32>,
    /// `HFN_SEL_COUNT(sel)`.
    pub count: i32,
}

/// Everything the two bodies need from the game and the host.
pub(crate) trait Prefab {
    /// The document's selection state, `None` when the document has no selection object.
    fn selection_state(&self, doc: Doc) -> Option<SelState>;
    /// `HFN_GET_ACTIVE_HISTORY` + `HFN_ADD_OPERATION(hist, doc->selection, label, 0)`.
    fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize;
    /// `HFN_RECORD_NODE(hist, node + IUndoable, flag)`.
    fn record_node(&self, hist: usize, node: Node, flag: i32);
    /// `HFN_DOC_ADD_PREFAB(doc, abs)`: the new `CMapPrefab`, `None` when it was not created.
    fn add_prefab(&self, doc: Doc, abs: &CStr) -> Option<Node>;
    /// `HFN_DOC_PREFAB_FROM_SELECTION(doc, abs, frame, centered)`.
    fn prefab_from_selection(&self, doc: Doc, abs: &CStr, frame: &[u8], centered: bool) -> Option<Node>;
    /// `node->vt->SetOrigin` (slot 17).
    fn set_origin(&self, node: Node, value: [f32; 3]);
    /// `node->vt->SetAngles` (slot 18).
    fn set_angles(&self, node: Node, value: [f32; 3]);
    /// `HFN_DOC_SET_MODIFIED(doc, doc)`.
    fn doc_set_modified(&self, doc: Doc);
    /// The node's `nodeID`.
    fn node_id(&self, node: Node) -> Option<i32>;
    /// The node's class name (`"prefab"` for a `CMapPrefab`).
    fn class(&self, node: Node) -> &'static str;
    /// The node's origin, read back.
    fn origin(&self, node: Node) -> Option<[f32; 3]>;
    /// The node's angles, read back.
    fn angles(&self, node: Node) -> Option<[f32; 3]>;
    /// `HFN_PREFAB_GET_TARGET_MAP(prefab)`: the content-relative path Hammer stored (empty when
    /// the path was rejected), copied out of the game string.
    fn target_map(&self, node: Node) -> Vec<u8>;
    /// `HFN_PREFAB_GET_WORLD(prefab) != 0`: the referenced map is loaded.
    fn has_world(&self, node: Node) -> bool;
    /// The raw exe path and command line `map.open`'s host gate derives the app root and the addon
    /// name from.
    fn process_facts(&self) -> (Option<String>, Option<String>);
    /// `CreateFileA` + `GetFileType` on the host path.
    fn file_fact(&self, host: &[u8]) -> HostFact;
    /// `map.open`'s binary-v9 DMX header gate.
    fn header_gate(&self, host: &[u8]) -> Result<(), OpErr>;
    /// The host path's parent directory exists.
    fn dir_exists(&self, host: &[u8]) -> bool;
    /// Some Hammer session holds this host path (an open map).
    fn map_is_open(&self, host: &[u8]) -> Result<bool, OpErr>;
}

fn invalid(reason: &str) -> OpErr {
    OpErr::new(-32602, "InvalidParams", reason)
}

/// `origin` / `angles` as `node.setTransform` validates them (finite triples; the origin also
/// range-checked).
fn parse_vec3(bytes: &[u8], key: &[u8], what: &str) -> Result<Option<[f32; 3]>, OpErr> {
    let value = json_float3_top(bytes, key);
    if json_find_top(bytes, key).is_some() && value.is_none() {
        return Err(invalid(what));
    }
    Ok(value)
}

/// The shared head: the addon-relative path, then the absolute host path through `map.open`'s own
/// host gate. `fact` receives what the file probe saw (the gate itself refuses a missing file,
/// which `prefabFromSelection` needs the opposite of, so the probe always says "disk" and the
/// caller judges).
fn resolve(p: &dyn Prefab, params: Option<&str>) -> Result<(Vec<u8>, Vec<u8>, HostFact), OpErr> {
    let rel = map::params_gate(params)?;
    let (exe, cmdline) = p.process_facts();
    let fact = core::cell::Cell::new(HostFact::NoSuchFile);
    let host = map::host_gate_with(exe.as_deref(), cmdline.as_deref(), &rel, |host| {
        fact.set(p.file_fact(host));
        HostFact::Disk
    })?;
    Ok((rel, host, fact.get()))
}

fn c_path(host: &[u8]) -> Result<CString, OpErr> {
    CString::new(host).map_err(|_| OpErr::new(-32603, "InternalError", "host path carries a NUL"))
}

fn open_map_err() -> OpErr {
    invalid("path: that map is open in Hammer (close it first, or pick another file)")
}

/// `{"targetMapPath":..,"loaded":..}` pieces both replies share, plus the warnings.
fn target_warning(rel: &[u8], target: &[u8], loaded: bool) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !target.eq_ignore_ascii_case(rel) {
        parts.push(format!(
            "targetMapPath '{}' differs from the requested path '{}'",
            String::from_utf8_lossy(target),
            String::from_utf8_lossy(rel)
        ));
    }
    if !loaded {
        parts.push(
            "the referenced map is not loaded (the prefab is empty in the live document); it may \
             load after map.save + map.reload"
                .to_string(),
        );
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn add_opt3(out: &mut Sb, v: Option<[f32; 3]>) {
    match v {
        Some(v) => addf3(out, v),
        None => out.add(b"null"),
    }
}

fn add_node_id(out: &mut Sb, id: Option<i32>) {
    match id {
        Some(id) => out.addi(i64::from(id)),
        None => out.add(b"null"),
    }
}

/// `node.createPrefab`: add a prefab instance of an existing addon map.
pub(crate) fn op_node_create_prefab(
    p: &dyn Prefab,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (rel, host, fact) = resolve(p, params)?;
    let bytes = params.map_or(&[][..], str::as_bytes);
    let origin = parse_vec3(bytes, b"origin", "origin: [x,y,z] required")?;
    let angles = parse_vec3(bytes, b"angles", "angles: [pitch,yaw,roll] required")?;
    if let Some(origin) = origin {
        if !xform_origin_ok(&origin) {
            let mut why = vec![0u8; 96];
            xform_origin_reason(&mut why);
            let end = why.iter().position(|&b| b == 0).unwrap_or(why.len());
            return Err(invalid(&String::from_utf8_lossy(&why[..end])));
        }
    }
    // The file must be a real, binary-v9 vmap (the same gates map.open runs before OpenFile).
    match fact {
        HostFact::Disk => {}
        HostFact::NoSuchFile => return Err(OpErr::new(-32014, "AssetNotFound", "no_such_file")),
        HostFact::NotAFile => return Err(OpErr::new(-32014, "AssetNotFound", "not_a_file")),
    }
    p.header_gate(&host)?;
    // An open map is a registered map: the document the prefab would live in, or another session.
    if p.map_is_open(&host)? {
        return Err(open_map_err());
    }
    let abs = c_path(&host)?;
    let hist = p.undo_open(doc, CREATE_LABEL);
    let Some(prefab) = p.add_prefab(doc, &abs) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "AddPrefab created no node (see bridge log); nothing was added",
        ));
    };
    p.record_node(hist, prefab, 0);
    if let Some(origin) = origin {
        p.set_origin(prefab, origin);
    }
    if let Some(angles) = angles {
        p.set_angles(prefab, angles);
    }
    p.doc_set_modified(doc);
    let target = p.target_map(prefab);
    if target.is_empty() {
        // Hammer silently dropped the path (RE notes section 2): an empty prefab is in the
        // document and recorded in the undo group.
        return Err(OpErr::maybe(
            -32603,
            "PrefabPathRejected",
            "Hammer stored no target map path for the new prefab (an empty prefab was added; \
             undo it with undo.undo)",
        ));
    }
    let loaded = p.has_world(prefab);
    out.add(b"{\"nodeID\":");
    add_node_id(out, p.node_id(prefab));
    out.add(b",\"class\":");
    out.addjson(p.class(prefab).as_bytes());
    out.add(b",\"targetMapPath\":");
    out.addjson(&target);
    out.add(b",\"loaded\":");
    out.add(if loaded { b"true" } else { b"false" });
    out.add(b",\"origin\":");
    add_opt3(out, p.origin(prefab));
    out.add(b",\"angles\":");
    add_opt3(out, p.angles(prefab));
    out.add(b",\"undoLabel\":\"MCP: create prefab\"");
    if let Some(warning) = target_warning(&rel, &target, loaded) {
        out.add(b",\"warning\":");
        out.addjson(warning.as_bytes());
    }
    out.add(b"}");
    Ok(())
}

/// `node.prefabFromSelection`: turn the selection into a new prefab file.
pub(crate) fn op_node_prefab_from_selection(
    p: &dyn Prefab,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (rel, host, fact) = resolve(p, params)?;
    let bytes = params.map_or(&[][..], str::as_bytes);
    let centered = match serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("centered").cloned())
    {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => b,
        Some(_) => return Err(invalid("centered: a boolean is required")),
    };
    // The worker overwrites without asking: the target must not exist, and its directory must.
    match fact {
        HostFact::NoSuchFile => {}
        HostFact::Disk | HostFact::NotAFile => {
            return Err(invalid(
                "path: that file already exists (the worker would overwrite it; pick a new name)",
            ));
        }
    }
    if !p.dir_exists(&host) {
        return Err(OpErr::new(
            -32014,
            "AssetNotFound",
            "directory_missing: create the target directory first",
        ));
    }
    if p.map_is_open(&host)? {
        return Err(open_map_err());
    }
    // The UI's own preconditions (RE notes section 3), before anything opens.
    let Some(state) = p.selection_state(doc) else {
        return Err(OpErr::new(-32603, "InternalError", "no selection object"));
    };
    if state.count <= 0 {
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "selection_empty: select the nodes first"));
    }
    match state.mode {
        Some(mode) if (0..=MODE_MAX).contains(&mode) => {}
        _ => {
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "selection_mode: the selection mode must be groups, objects or meshes",
            ));
        }
    }
    let abs = c_path(&host)?;
    let frame = [0u8; FRAME_LEN];
    p.undo_open(doc, FROM_SELECTION_LABEL);
    let Some(prefab) = p.prefab_from_selection(doc, &abs, &frame, centered) else {
        return Err(OpErr::maybe(
            -32603,
            "InternalError",
            "the prefab worker returned no node (see bridge log); the group may hold partial changes",
        ));
    };
    p.doc_set_modified(doc);
    let target = p.target_map(prefab);
    let loaded = p.has_world(prefab);
    let written = p.file_fact(&host) == HostFact::Disk;
    out.add(b"{\"nodeID\":");
    add_node_id(out, p.node_id(prefab));
    out.add(b",\"class\":");
    out.addjson(p.class(prefab).as_bytes());
    out.add(b",\"targetMapPath\":");
    out.addjson(&target);
    out.add(b",\"written\":");
    out.add(if written { b"true" } else { b"false" });
    out.add(b",\"loaded\":");
    out.add(if loaded { b"true" } else { b"false" });
    out.add(b",\"selectedCount\":");
    out.addi(i64::from(state.count));
    out.add(b",\"centered\":");
    out.add(if centered { b"true" } else { b"false" });
    out.add(b",\"fileStaysOnUndo\":true,\"undoLabel\":\"MCP: create prefab from selection\"");
    let mut warning = target_warning(&rel, &target, loaded).unwrap_or_default();
    if !written {
        if !warning.is_empty() {
            warning.push_str("; ");
        }
        warning.push_str("the worker returned a prefab but the file does not exist on disk");
    }
    if !warning.is_empty() {
        out.add(b",\"warning\":");
        out.addjson(warning.as_bytes());
    }
    out.add(b"}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The dispatch arms
// ---------------------------------------------------------------------------------------------

type PrefabOp = fn(&dyn Prefab, Doc, Option<&str>, &mut Sb) -> Result<(), OpErr>;

fn prefab_handler(req: &Request, op: PrefabOp) -> String {
    let mut buf = vec![0u8; REPLY_CAP];
    let mut out = Sb::new(&mut buf);
    let w = crate::ops::hammer::game_write();
    let params = req.params.as_deref();
    let live = GamePrefab;
    let outcome = crate::ops::hammer::write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op(&live, doc, params, &mut out).map(|()| (doc, false)));
    crate::ops::hammer::write_finish(&req.id, outcome, &mut out, "response too large")
}

fn node_create_prefab(req: &Request) -> String {
    prefab_handler(req, op_node_create_prefab)
}

fn node_prefab_from_selection(req: &Request) -> String {
    prefab_handler(req, op_node_prefab_from_selection)
}

/// The rows this group serves.
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("node.createPrefab", node_create_prefab),
    ("node.prefabFromSelection", node_prefab_from_selection),
];

// ---------------------------------------------------------------------------------------------
// The production host
// ---------------------------------------------------------------------------------------------

/// The production host: `ops::hammer`'s live writer for the generic steps, the five bound pins for
/// the prefab ones.
pub(crate) struct GamePrefab;

#[cfg(windows)]
mod live {
    use core::ffi::{c_void, CStr};

    use s2sdk_sys::generated::CMapNode;

    use super::{GamePrefab, Prefab, SelState, TARGET_CAP};
    use crate::generated::pins;
    use crate::ops::hammer::bind::{hfn, HammerFns};
    use crate::ops::hammer::{game_write, Doc, Node};
    use crate::ops::map::{self, HostFact};
    use crate::ops::OpErr;

    fn table() -> HammerFns {
        hfn().expect("cs2-bridge: the hammer function table is bound before any prefab call")
    }

    /// `CMapNode_Undoable`: the `IUndoable` subobject at the generated base offset (the same
    /// computation `ops::mesh` and `ops::hammer` make for their `RecordNode*` calls).
    fn undoable(node: Node) -> *mut c_void {
        // SAFETY: the base offset is generated from the same IR; `node` came from a prefab
        // worker or the identity-checked tree.
        unsafe { (node.0 as *mut u8).add(CMapNode::BASE_OFFSET_IUndoable).cast::<c_void>() }
    }

    impl Prefab for GamePrefab {
        fn selection_state(&self, doc: Doc) -> Option<SelState> {
            let w = game_write();
            let sel = w.selection(doc.0);
            if sel == 0 || !w.is_of(sel, pins::HV_SELECTION_VT.value as u32) {
                return None;
            }
            let t = table();
            // SAFETY: verified HFN_SEL_CURRENT_SET / HFN_SEL_GET_SET / HFN_SEL_COUNT over the
            // identity-checked selection object; the mode index stays in 0..8 (the same scan
            // `selection.get` makes).
            unsafe {
                let current = (t.sel_current_set)(sel as *mut c_void) as usize;
                let mode = if current == 0 {
                    None
                } else {
                    (0..8).find(|&k| (t.sel_get_set)(sel as *mut c_void, k) as usize == current)
                };
                Some(SelState { mode, count: (t.sel_count)(sel as *mut c_void) })
            }
        }

        fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize {
            game_write().undo_open(doc, label)
        }

        fn record_node(&self, hist: usize, node: Node, flag: i32) {
            // SAFETY: `hist` is `undo_open`'s token, `undoable` the node's IUndoable subobject.
            unsafe { (table().record_node)(hist as *mut c_void, undoable(node), flag) };
        }

        fn add_prefab(&self, doc: Doc, abs: &CStr) -> Option<Node> {
            // SAFETY: verified HFN_DOC_ADD_PREFAB `(CMapDoc*, const char*) -> CMapPrefab*`; the
            // document passed the write gate and `abs` outlives the call.
            let p = unsafe { (table().doc_add_prefab)(doc.0 as *mut c_void, abs.as_ptr()) };
            (!p.is_null()).then_some(Node(p as usize))
        }

        fn prefab_from_selection(
            &self,
            doc: Doc,
            abs: &CStr,
            frame: &[u8],
            centered: bool,
        ) -> Option<Node> {
            assert!(frame.len() >= super::FRAME_LEN, "cs2-bridge: prefab frame buffer too small");
            // SAFETY: verified HFN_DOC_PREFAB_FROM_SELECTION `(CMapDoc*, const char*, const Frame*,
            // bool) -> CMapPrefab*`; `frame` is a zero-filled buffer of `FRAME_LEN` bytes (>= the
            // 0xB4 the worker's only read, a matrix at +0x24, needs); `abs` outlives the call.
            let p = unsafe {
                (table().doc_prefab_from_selection)(
                    doc.0 as *mut c_void,
                    abs.as_ptr(),
                    frame.as_ptr().cast(),
                    u8::from(centered),
                )
            };
            (!p.is_null()).then_some(Node(p as usize))
        }

        fn set_origin(&self, node: Node, value: [f32; 3]) {
            game_write().set_origin(node, value);
        }

        fn set_angles(&self, node: Node, value: [f32; 3]) {
            game_write().set_angles(node, value);
        }

        fn doc_set_modified(&self, doc: Doc) {
            game_write().doc_set_modified(doc);
        }

        fn node_id(&self, node: Node) -> Option<i32> {
            game_write().id(node)
        }

        fn class(&self, node: Node) -> &'static str {
            game_write().class(node)
        }

        fn origin(&self, node: Node) -> Option<[f32; 3]> {
            game_write().origin(node)
        }

        fn angles(&self, node: Node) -> Option<[f32; 3]> {
            game_write().angles(node)
        }

        fn target_map(&self, node: Node) -> Vec<u8> {
            // SAFETY: verified HFN_PREFAB_GET_TARGET_MAP: returns the prefab's CUtlString data
            // or a static empty string; read through the probing string copy.
            let s = unsafe { (table().prefab_get_target_map)(node.0 as *mut c_void) };
            let mut buf = vec![0u8; TARGET_CAP];
            // SAFETY: `md_str` only reads `s` through `mem_ok`.
            unsafe { map::md_str(s.cast(), &mut buf) };
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            buf.truncate(end);
            buf
        }

        fn has_world(&self, node: Node) -> bool {
            // SAFETY: verified HFN_PREFAB_GET_WORLD over a prefab node.
            !unsafe { (table().prefab_get_world)(node.0 as *mut c_void) }.is_null()
        }

        fn process_facts(&self) -> (Option<String>, Option<String>) {
            map::raw_process_facts()
        }

        fn file_fact(&self, host: &[u8]) -> HostFact {
            map::file_fact(host)
        }

        fn header_gate(&self, host: &[u8]) -> Result<(), OpErr> {
            map::header_gate(host)
        }

        fn dir_exists(&self, host: &[u8]) -> bool {
            let text = String::from_utf8_lossy(host).into_owned();
            std::path::Path::new(&text).parent().is_some_and(std::path::Path::is_dir)
        }

        fn map_is_open(&self, host: &[u8]) -> Result<bool, OpErr> {
            map::path_is_open(host)
        }
    }
}

/// The non-Windows build has no Hammer: the `-32017` module gate answers before a body runs.
#[cfg(not(windows))]
impl Prefab for GamePrefab {
    fn selection_state(&self, _doc: Doc) -> Option<SelState> {
        no_prefab("selection")
    }
    fn undo_open(&self, _doc: Doc, _label: &'static CStr) -> usize {
        no_prefab("undo")
    }
    fn record_node(&self, _hist: usize, _node: Node, _flag: i32) {
        no_prefab("record")
    }
    fn add_prefab(&self, _doc: Doc, _abs: &CStr) -> Option<Node> {
        no_prefab("AddPrefab")
    }
    fn prefab_from_selection(&self, _doc: Doc, _abs: &CStr, _frame: &[u8], _centered: bool) -> Option<Node> {
        no_prefab("PrefabFromSelection")
    }
    fn set_origin(&self, _node: Node, _value: [f32; 3]) {
        no_prefab("origin")
    }
    fn set_angles(&self, _node: Node, _value: [f32; 3]) {
        no_prefab("angles")
    }
    fn doc_set_modified(&self, _doc: Doc) {
        no_prefab("modified")
    }
    fn node_id(&self, _node: Node) -> Option<i32> {
        no_prefab("id")
    }
    fn class(&self, _node: Node) -> &'static str {
        no_prefab("class")
    }
    fn origin(&self, _node: Node) -> Option<[f32; 3]> {
        no_prefab("origin")
    }
    fn angles(&self, _node: Node) -> Option<[f32; 3]> {
        no_prefab("angles")
    }
    fn target_map(&self, _node: Node) -> Vec<u8> {
        no_prefab("target")
    }
    fn has_world(&self, _node: Node) -> bool {
        no_prefab("world")
    }
    fn process_facts(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }
    fn file_fact(&self, _host: &[u8]) -> HostFact {
        HostFact::NoSuchFile
    }
    fn header_gate(&self, _host: &[u8]) -> Result<(), OpErr> {
        Ok(())
    }
    fn dir_exists(&self, _host: &[u8]) -> bool {
        false
    }
    fn map_is_open(&self, _host: &[u8]) -> Result<bool, OpErr> {
        Ok(false)
    }
}

#[cfg(not(windows))]
fn no_prefab(what: &str) -> ! {
    panic!("cs2-bridge: prefab {what} needs the Windows tools build (Hammer is a Windows module)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const PREFAB: Node = Node(0xB000);
    const DOC: Doc = Doc(0xD000);
    const EXE: &str = r"C:\game\bin\win64\cs2.exe";
    const CMDLINE: &str = r#"cs2.exe -tools -addon myaddon"#;

    /// A scripted host: the facts the pre-checks read and the answers the workers give, with every
    /// call logged in order.
    struct Fake {
        calls: RefCell<Vec<String>>,
        /// What the file probe answers for the FIRST call; after a worker ran it answers
        /// `file_after` (the worker writes the file).
        file: HostFact,
        file_after: HostFact,
        worker_ran: RefCell<bool>,
        header: Result<(), OpErr>,
        dir: bool,
        open: Result<bool, OpErr>,
        selection: Option<SelState>,
        add: bool,
        from_sel: bool,
        target: Vec<u8>,
        world: bool,
        process: (Option<String>, Option<String>),
        frame_seen: RefCell<Vec<u8>>,
        centered_seen: RefCell<Option<bool>>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                file: HostFact::Disk,
                file_after: HostFact::Disk,
                worker_ran: RefCell::new(false),
                header: Ok(()),
                dir: true,
                open: Ok(false),
                selection: Some(SelState { mode: Some(1), count: 3 }),
                add: true,
                from_sel: true,
                target: b"maps/sub/inner.vmap".to_vec(),
                world: true,
                process: (Some(EXE.to_string()), Some(CMDLINE.to_string())),
                frame_seen: RefCell::new(Vec::new()),
                centered_seen: RefCell::new(None),
            }
        }
    }

    impl Fake {
        fn log(&self, line: impl Into<String>) {
            self.calls.borrow_mut().push(line.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Prefab for Fake {
        fn selection_state(&self, _doc: Doc) -> Option<SelState> {
            self.log("selection_state");
            self.selection
        }
        fn undo_open(&self, _doc: Doc, label: &'static CStr) -> usize {
            self.log(format!("undo_open {}", label.to_str().unwrap()));
            0x7000
        }
        fn record_node(&self, hist: usize, node: Node, flag: i32) {
            self.log(format!("record_node {hist:#x} {:#x} flag={flag}", node.0));
        }
        fn add_prefab(&self, _doc: Doc, abs: &CStr) -> Option<Node> {
            self.log(format!("add_prefab {}", abs.to_str().unwrap()));
            *self.worker_ran.borrow_mut() = true;
            self.add.then_some(PREFAB)
        }
        fn prefab_from_selection(&self, _doc: Doc, abs: &CStr, frame: &[u8], centered: bool) -> Option<Node> {
            self.log(format!("from_selection {} centered={centered}", abs.to_str().unwrap()));
            *self.frame_seen.borrow_mut() = frame.to_vec();
            *self.centered_seen.borrow_mut() = Some(centered);
            *self.worker_ran.borrow_mut() = true;
            self.from_sel.then_some(PREFAB)
        }
        fn set_origin(&self, _node: Node, value: [f32; 3]) {
            self.log(format!("set_origin {value:?}"));
        }
        fn set_angles(&self, _node: Node, value: [f32; 3]) {
            self.log(format!("set_angles {value:?}"));
        }
        fn doc_set_modified(&self, _doc: Doc) {
            self.log("doc_set_modified");
        }
        fn node_id(&self, _node: Node) -> Option<i32> {
            Some(77)
        }
        fn class(&self, _node: Node) -> &'static str {
            "prefab"
        }
        fn origin(&self, _node: Node) -> Option<[f32; 3]> {
            Some([1.0, 2.0, 3.0])
        }
        fn angles(&self, _node: Node) -> Option<[f32; 3]> {
            Some([0.0, 90.0, 0.0])
        }
        fn target_map(&self, _node: Node) -> Vec<u8> {
            self.log("target_map");
            self.target.clone()
        }
        fn has_world(&self, _node: Node) -> bool {
            self.log("has_world");
            self.world
        }
        fn process_facts(&self) -> (Option<String>, Option<String>) {
            self.process.clone()
        }
        fn file_fact(&self, host: &[u8]) -> HostFact {
            self.log(format!("file_fact {}", String::from_utf8_lossy(host)));
            if *self.worker_ran.borrow() {
                self.file_after
            } else {
                self.file
            }
        }
        fn header_gate(&self, _host: &[u8]) -> Result<(), OpErr> {
            self.log("header_gate");
            self.header.clone()
        }
        fn dir_exists(&self, _host: &[u8]) -> bool {
            self.dir
        }
        fn map_is_open(&self, _host: &[u8]) -> Result<bool, OpErr> {
            self.log("map_is_open");
            self.open.clone()
        }
    }

    fn run(op: PrefabOp, fake: &Fake, params: &str) -> Result<String, OpErr> {
        let mut buf = vec![0u8; REPLY_CAP];
        let mut out = Sb::new(&mut buf);
        op(fake, DOC, Some(params), &mut out)?;
        Ok(String::from_utf8_lossy(out.as_cstr()).into_owned())
    }

    fn idx(calls: &[String], prefix: &str) -> usize {
        calls
            .iter()
            .position(|c| c.starts_with(prefix))
            .unwrap_or_else(|| panic!("no `{prefix}` in {calls:?}"))
    }

    fn reason(e: &OpErr) -> String {
        e.data.clone()
    }

    #[test]
    fn create_prefab_runs_the_import_sequence_in_order() {
        let fake = Fake::default();
        let reply = run(
            op_node_create_prefab,
            &fake,
            r#"{"path":"maps/sub/inner.vmap","origin":[1,2,3],"angles":[0,90,0]}"#,
        )
        .unwrap();
        assert_eq!(
            reply,
            r#"{"nodeID":77,"class":"prefab","targetMapPath":"maps/sub/inner.vmap","loaded":true,"origin":[1,2,3],"angles":[0,90,0],"undoLabel":"MCP: create prefab"}"#
        );
        let calls = fake.calls();
        // The abs path is the content path under the addon, derived like map.open.
        let add = &calls[idx(&calls, "add_prefab")];
        assert!(add.contains("content") && add.contains("csgo_addons") && add.contains("myaddon"), "{add}");
        assert!(add.ends_with("maps\\sub\\inner.vmap") || add.ends_with("maps/sub/inner.vmap"), "{add}");
        let order = [
            idx(&calls, "header_gate"),
            idx(&calls, "map_is_open"),
            idx(&calls, "undo_open MCP: create prefab"),
            idx(&calls, "add_prefab"),
            idx(&calls, "record_node 0x7000 0xb000 flag=0"),
            idx(&calls, "set_origin"),
            idx(&calls, "set_angles"),
            idx(&calls, "doc_set_modified"),
            idx(&calls, "target_map"),
            idx(&calls, "has_world"),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{calls:?}");
    }

    #[test]
    fn create_prefab_without_transform_writes_none_and_warns_when_not_loaded() {
        let fake = Fake { world: false, ..Fake::default() };
        let reply = run(op_node_create_prefab, &fake, r#"{"path":"maps/sub/inner.vmap"}"#).unwrap();
        assert!(reply.contains(r#""loaded":false"#), "{reply}");
        assert!(reply.contains(r#""warning":"the referenced map is not loaded"#), "{reply}");
        let calls = fake.calls();
        assert!(!calls.iter().any(|c| c.starts_with("set_origin") || c.starts_with("set_angles")));
        // A different stored path is reported too.
        let fake = Fake { target: b"maps/other.vmap".to_vec(), ..Fake::default() };
        let reply = run(op_node_create_prefab, &fake, r#"{"path":"maps/sub/inner.vmap"}"#).unwrap();
        assert!(reply.contains("differs from the requested path"), "{reply}");
        // Case differences are not a difference (Hammer compares case-insensitively).
        let fake = Fake { target: b"MAPS/SUB/INNER.VMAP".to_vec(), ..Fake::default() };
        let reply = run(op_node_create_prefab, &fake, r#"{"path":"maps/sub/inner.vmap"}"#).unwrap();
        assert!(!reply.contains("warning"), "{reply}");
    }

    #[test]
    fn create_prefab_param_errors_touch_nothing() {
        let cases: &[(&str, &str)] = &[
            (r#"{}"#, "path: relative to the addon content root"),
            (r#"{"path":""}"#, "path: relative to the addon content root"),
            (r#"{"path":"/abs/maps/x.vmap"}"#, "path"),
            (r#"{"path":"maps/../x.vmap"}"#, "path"),
            (r#"{"path":"maps/x.txt"}"#, "path"),
            (r#"{"path":"models/x.vmap"}"#, "path"),
            (r#"{"path":"maps/a\\b.vmap"}"#, "no JSON escapes"),
            (r#"{"path":"maps/x.vmap","origin":[1,2]}"#, "origin: [x,y,z] required"),
            (r#"{"path":"maps/x.vmap","angles":"up"}"#, "angles: [pitch,yaw,roll] required"),
            (r#"{"path":"maps/x.vmap","origin":[1e30,0,0]}"#, ""),
        ];
        for (json, want) in cases {
            let fake = Fake::default();
            let e = run(op_node_create_prefab, &fake, json).unwrap_err();
            assert_eq!(e.code, -32602, "{json}: {e:?}");
            assert!(reason(&e).contains(want), "{json}: {e:?}");
            let calls = fake.calls();
            assert!(
                !calls.iter().any(|c| c.starts_with("undo_open") || c.starts_with("add_prefab")),
                "{json}: {calls:?}"
            );
        }
    }

    #[test]
    fn create_prefab_refuses_missing_files_bad_headers_and_open_maps_before_the_group() {
        let fake = Fake { file: HostFact::NoSuchFile, ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32014, "AssetNotFound"));
        assert!(!fake.calls().iter().any(|c| c.starts_with("undo_open")));

        let fake = Fake { file: HostFact::NotAFile, ..Fake::default() };
        assert_eq!(run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err().code, -32014);

        let fake = Fake {
            header: Err(OpErr::new(-32014, "AssetNotFound", "InvalidMapFile")),
            ..Fake::default()
        };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!(e.data, r#"{"reason":"InvalidMapFile"}"#);
        assert!(!fake.calls().iter().any(|c| c.starts_with("undo_open")));

        let fake = Fake { open: Ok(true), ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!(e.code, -32602);
        assert!(reason(&e).contains("is open in Hammer"), "{e:?}");
        assert!(!fake.calls().iter().any(|c| c.starts_with("undo_open")));

        let fake = Fake {
            open: Err(OpErr::new(-32603, "InternalError", "session_vector_unreadable")),
            ..Fake::default()
        };
        assert_eq!(run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err().code, -32603);
    }

    #[test]
    fn create_prefab_needs_the_process_facts_like_map_open() {
        let fake = Fake { process: (None, Some(CMDLINE.to_string())), ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32017, "PrerequisiteFailed"));
        let fake = Fake { process: (Some(EXE.to_string()), None), ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32017, "PrerequisiteFailed"));
    }

    #[test]
    fn create_prefab_null_worker_result_is_an_error_with_the_group_open_and_nothing_recorded() {
        let fake = Fake { add: false, ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32603, "InternalError"));
        let calls = fake.calls();
        assert!(calls.iter().any(|c| c.starts_with("add_prefab")));
        assert!(!calls.iter().any(|c| c.starts_with("record_node")), "{calls:?}");
    }

    #[test]
    fn create_prefab_with_a_dropped_path_is_a_maybe_executed_error() {
        let fake = Fake { target: Vec::new(), ..Fake::default() };
        let e = run(op_node_create_prefab, &fake, r#"{"path":"maps/x.vmap"}"#).unwrap_err();
        assert_eq!(e.name, "PrefabPathRejected");
        assert!(e.data.contains(r#""maybeExecuted":true"#), "{e:?}");
        // It WAS recorded, so undo.undo removes it.
        assert!(fake.calls().iter().any(|c| c.starts_with("record_node")));
    }

    #[test]
    fn from_selection_runs_the_save_prefab_sequence_in_order() {
        let fake = Fake { file: HostFact::NoSuchFile, file_after: HostFact::Disk, ..Fake::default() };
        let reply = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/sub/new.vmap","centered":true}"#).unwrap();
        assert_eq!(
            reply,
            r#"{"nodeID":77,"class":"prefab","targetMapPath":"maps/sub/inner.vmap","written":true,"loaded":true,"selectedCount":3,"centered":true,"fileStaysOnUndo":true,"undoLabel":"MCP: create prefab from selection","warning":"targetMapPath 'maps/sub/inner.vmap' differs from the requested path 'maps/sub/new.vmap'"}"#
        );
        let calls = fake.calls();
        let order = [
            idx(&calls, "map_is_open"),
            idx(&calls, "selection_state"),
            idx(&calls, "undo_open MCP: create prefab from selection"),
            idx(&calls, "from_selection"),
            idx(&calls, "doc_set_modified"),
            idx(&calls, "target_map"),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{calls:?}");
        // The file was probed before the worker (absent) and after it (written).
        assert_eq!(calls.iter().filter(|c| c.starts_with("file_fact")).count(), 2);
        assert!(calls[idx(&calls, "from_selection")].ends_with("centered=true"));
        // The frame is a zeroed buffer of FRAME_LEN bytes.
        let frame = fake.frame_seen.borrow();
        assert_eq!(frame.len(), FRAME_LEN);
        assert!(frame.iter().all(|&b| b == 0));
        // No record_node: the worker records its own prefab.
        assert!(!calls.iter().any(|c| c.starts_with("record_node")));
    }

    #[test]
    fn from_selection_defaults_to_world_offset_and_rejects_a_non_bool() {
        let fake = Fake { file: HostFact::NoSuchFile, ..Fake::default() };
        let reply = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap","centered":null}"#).unwrap();
        assert!(reply.contains(r#""centered":false"#), "{reply}");
        assert_eq!(*fake.centered_seen.borrow(), Some(false));
        let fake = Fake { file: HostFact::NoSuchFile, ..Fake::default() };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap","centered":"yes"}"#).unwrap_err();
        assert_eq!(e.code, -32602);
        assert!(!fake.calls().iter().any(|c| c.starts_with("undo_open")));
    }

    #[test]
    fn from_selection_pre_checks_refuse_before_the_group_opens() {
        let no_group = |fake: &Fake| !fake.calls().iter().any(|c| c.starts_with("undo_open") || c.starts_with("from_selection"));

        // The target exists: the worker would overwrite it.
        let fake = Fake::default();
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert_eq!(e.code, -32602);
        assert!(reason(&e).contains("already exists"), "{e:?}");
        assert!(no_group(&fake));

        // Missing directory.
        let fake = Fake { file: HostFact::NoSuchFile, dir: false, ..Fake::default() };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32014, "AssetNotFound"));
        assert!(reason(&e).contains("directory_missing"));
        assert!(no_group(&fake));

        // An open map.
        let fake = Fake { file: HostFact::NoSuchFile, open: Ok(true), ..Fake::default() };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert!(reason(&e).contains("is open in Hammer"), "{e:?}");
        assert!(no_group(&fake));

        // Empty selection.
        let fake = Fake {
            file: HostFact::NoSuchFile,
            selection: Some(SelState { mode: Some(1), count: 0 }),
            ..Fake::default()
        };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32017, "PrerequisiteFailed"));
        assert!(reason(&e).contains("selection_empty"));
        assert!(no_group(&fake));

        // Wrong selection modes (3 = faces ... 7, and no matching mode at all).
        for mode in [Some(3), Some(5), Some(7), None] {
            let fake = Fake {
                file: HostFact::NoSuchFile,
                selection: Some(SelState { mode, count: 2 }),
                ..Fake::default()
            };
            let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
            assert!(reason(&e).contains("selection_mode"), "{mode:?}: {e:?}");
            assert!(no_group(&fake));
        }
        for mode in [0, 1, 2] {
            let fake = Fake {
                file: HostFact::NoSuchFile,
                selection: Some(SelState { mode: Some(mode), count: 2 }),
                ..Fake::default()
            };
            assert!(run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).is_ok(), "{mode}");
        }

        // No selection object.
        let fake = Fake { file: HostFact::NoSuchFile, selection: None, ..Fake::default() };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert_eq!(e.code, -32603);
        assert!(no_group(&fake));

        // Path shape and addon containment are map.open's.
        for bad in [r#"{"path":"../x.vmap"}"#, r#"{"path":"C:/x/maps/a.vmap"}"#, r#"{"path":"maps/a.vmap/../../b.vmap"}"#, r#"{}"#] {
            let fake = Fake { file: HostFact::NoSuchFile, ..Fake::default() };
            let e = run(op_node_prefab_from_selection, &fake, bad).unwrap_err();
            assert_eq!(e.code, -32602, "{bad}");
            assert!(no_group(&fake));
        }
    }

    #[test]
    fn from_selection_null_worker_result_is_maybe_executed() {
        let fake = Fake { file: HostFact::NoSuchFile, from_sel: false, ..Fake::default() };
        let e = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap_err();
        assert_eq!((e.code, e.name), (-32603, "InternalError"));
        assert!(e.data.contains(r#""maybeExecuted":true"#), "{e:?}");
        // The group was open when the worker ran.
        let calls = fake.calls();
        assert!(idx(&calls, "undo_open") < idx(&calls, "from_selection"));
    }

    #[test]
    fn from_selection_reports_a_missing_file_after_the_worker() {
        let fake = Fake { file: HostFact::NoSuchFile, file_after: HostFact::NoSuchFile, world: false, ..Fake::default() };
        let reply = run(op_node_prefab_from_selection, &fake, r#"{"path":"maps/new.vmap"}"#).unwrap();
        assert!(reply.contains(r#""written":false"#), "{reply}");
        assert!(reply.contains("does not exist on disk"), "{reply}");
        assert!(reply.contains(r#""loaded":false"#), "{reply}");
    }

    #[test]
    fn the_rows_are_registered_gated_on_hammer_and_served_here() {
        for name in ["node.createPrefab", "node.prefabFromSelection"] {
            let row = crate::registry::find(name).expect("registered");
            assert!(row.implemented && row.blocked_on.is_none(), "{name}");
            assert_eq!(row.needs, crate::registry::NEED_HAMMER, "{name}");
            assert!(HANDLERS.iter().any(|(n, _)| *n == name), "{name}");
        }
    }
}
