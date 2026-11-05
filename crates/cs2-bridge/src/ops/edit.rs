//! The EDIT lane (M4): `edit.batch`, `entity.tieMeshes`, `entity.untie` — the m2.19 tie/untie
//! recipes and the batch runner, ported 1:1 from the frozen C bridge:
//!
//! * `edit.batch` (the arm `bridge/mcp_bridge.c:5855-5970`): the write gate, the `ops` member,
//!   ONE undo group per batch (`batch_undo_label`, `:2468-2471`), the `"$N"` handle substitution
//!   (`batch_subst_handle`, `:2474-2504`), the per-op dispatch to [`hammer::op_entity_create_no_group`]
//!   and [`hammer::op_set_keyvalues_no_group`] (the C's own `op_*` bodies; unlike the standalone
//!   arms they open NO undo group of their own — the batch's group covers them), the 16-entry
//!   handle table, the 32-entry `recorded` array and the `results` / `failedIndex` reply shapes.
//! * `entity.tieMeshes` (`:4113-4431` + `tie_reply_body` `:4432-4482`): the whole param
//!   validation and the mesh gates BEFORE `undo_open("MCP: tie meshes to entity")` (the
//!   review-13 rule), the all-alreadyTied no-op without a group (FIX-B live L1), the reparent
//!   through `RecordNodesAndParents` + one `AddChild` (vtable slot 57) per mesh, the
//!   emptied-old-brush-entity deletion, and the two in-band failures (the defensive solid-class
//!   check and an AddChild refusal after earlier ties ran — both carry
//!   `undoGroupOpened`/`maybeExecuted` data).
//! * `entity.untie` (`:4485-4564`): Hammer's own `CMapDoc::MoveToWorld` with a 1-entry vector —
//!   it opens its OWN undo group, so this op never calls `undo_open`, collects the children
//!   (skipping unreadable and negative nodeIDs, `:4532-4534`) for the reply BEFORE the call and
//!   never touches the entity again.
//!
//! # How it is split
//!
//! The JSON builders, every parameter gate and the reply/error shapes are plain functions over
//! strings, numbers and small structs, exactly like [`crate::ops::hammer`]. The reads and calls
//! the bodies reach beyond the Hammer lane's [`hammer::Write`] trait — `node_parent`, `gd_type`,
//! `node_child_count`, the `AddChild` vtable slot, `RecordNodesAndParents`, `DocMoveToWorld` and
//! the classname read — sit behind this module's [`Edit`] trait, implemented for the hammer
//! lane's live reader ([`hammer::win::LiveHammer`]) in [`live`] (Windows tools build only) with
//! a recording fake for the host tests.
//!
//! # Statics
//!
//! The C keeps the batch's scratch (`results[256 * 1024]`, `opbuf[8192]`, `pbuf[8192]`,
//! `rbuf[65536]`), the tie's `kvbuf[8192]` and its id/node arrays in `static`s. They are
//! rewritten on every call, which happens on the tools main thread only (the tick's drain); this
//! port uses heap buffers fresh per call, which cannot change a byte on the wire.

use core::ffi::CStr;

use crate::control;
#[cfg(test)]
use crate::generated::pins;
use crate::entity_json::{
    batch_has_create, batch_op_next, batch_record_decision, json_find_top, json_int_top,
    json_string, json_string_top, Sb,
};
use crate::log;
use crate::ops::hammer;
use crate::ops::hammer::{
    addf3, byte_at, cstr, ci_seq, error_reply, game_write, geo_entity_node, geo_find, jnum, jws, seq,
    write_finish, CreateTarget, Doc, Node, Write, CHILDREN_LIST_MAX, NODEID_MAX, OP_RESULT_CAP,
};
use crate::ops::OpErr;
use crate::queue::Request;

/// `TIE_MAX_MESHES` (`mcp_bridge.c:3993`).
const TIE_MAX_MESHES: usize = 64;

/// `TIE_ERR_LIST_CAP` (`mcp_bridge.c:4030`): the id-list cap inside the AddChild-refusal data
/// (the C's `e->data` is a fixed 512-byte buffer, so the progress lists stop at 10 entries
/// each; the full reply of a successful tie reports every id).
const TIE_ERR_LIST_CAP: usize = 10;

/// The C's `keyValues` prescan scratch caps (`tie_kv_prescan`, `mcp_bridge.c:2206`):
/// `char name[96], sval[KV_VALUE_MAX + 1], nval[96]`.
const TIE_KV_NAME_CAP: usize = 96;

/// The tie's undo-group label (`mcp_bridge.c:4336`), also `tie_reply_body`'s `undoLabel` and
/// `tie_err_undo_group`'s label. A static C string: `Write::undo_open` hands the pointer
/// straight to Hammer's `AddOperation`, which may keep it.
const TIE_LABEL: &CStr = c"MCP: tie meshes to entity";

/// The literal at `pos` is exactly `lit` (`lit_at`, `mcp_bridge.c:638-641`): a compare that
/// walks the literal only — the C answers "true" for anything that STARTS with the literal,
/// and this port keeps that. (`entity_json::lit_at` is the same function, private to that
/// module; this is the one reader the EDIT bodies need.)
#[inline]
fn lit_at(s: &[u8], pos: usize, lit: &[u8]) -> bool {
    lit.iter().enumerate().all(|(i, &byte)| byte_at(s, pos + i) == byte)
}

// ---------------------------------------------------------------------------------------------
// The game reads and calls this lane adds to the Hammer lane's `Write`
// ---------------------------------------------------------------------------------------------

/// The reads and calls the tie/untie bodies need beyond [`hammer::Write`]: one method per C
/// primitive, bound like [`hammer::Write`] is (main thread only, after the module gate).
///
/// `class(node)` / `is_entity(node)` (the [`hammer::Nodes`] supertrait) answer
/// `is_mesh` (`mcp_bridge.c:3326`, `obj_is(.., HV_MAPMESH_VT)`) and
/// `obj_is(.., HV_MAPENTITY_VT)` — the same predicates the C spells inline.
pub(crate) trait Edit: hammer::Nodes {
    /// `node_parent` (`:3353-3358`): the node's parent, `None` when the field or the pointer is
    /// unreadable (the tie gate then treats the mesh as under the world).
    fn parent(&self, node: Node) -> Option<Node>;
    /// `gd_type` (`:3342-3351`): the FGD class type (`FgdClass_t::m_nType`) of the node's
    /// gamedata class, 5 = solid (brush), 4 = point; 0 when unreadable — every read is
    /// `mem_ok`-guarded, so a corrupt pointer is "not a brush entity", never a fault.
    fn gd_type(&self, node: Node) -> i32;
    /// `node_child_count` (`:3360-3364`): the guarded child count, -1 when the field is
    /// unreadable (never guess a count).
    fn child_count(&self, node: Node) -> i32;
    /// `ent->vt->AddChild(ent, node)` (`:4386`, slot 57): the C's one-byte success flag, read
    /// as a byte (never a Rust `bool` out of game code).
    fn add_child(&self, ent: Node, node: Node) -> u8;
    /// `g_hfn.RecordNodesAndParents(hist, &v, 0)` (`:4378`): the reparent recording, BEFORE the
    /// AddChild calls — records each node and, when it has a parent, that parent
    /// non-recursively; this is what makes the reparent undoable.
    fn record_nodes_and_parents(&self, hist: usize, nodes: &[Node]);
    /// `g_hfn.DocMoveToWorld(doc, &v)` (`:4547`) over the C's 1-entry vector: it opens its own
    /// undo group ("Untie from entity").
    fn doc_move_to_world(&self, doc: Doc, node: Node);
    /// `g_hfn.GetEntityProperties(node)` + `child_string(props, "classname", cap)`
    /// (`:4445-4446`): the entity's classname text, empty when the properties element is
    /// missing or unreadable (the C's `cn[0] = 0` fallback).
    fn classname(&self, node: Node, cap: usize) -> Vec<u8>;
}

// ---------------------------------------------------------------------------------------------
// The tie's error builders (`tie_err_node` / `tie_err_undo_group`)
// ---------------------------------------------------------------------------------------------

/// `tie_err_node` (`mcp_bridge.c:3998-4009`): the error data names the node the refusal is
/// about (plan-triggers.md §5: NodeNotFound {nodeID}, "not a mesh" {nodeID}, "already tied"
/// {nodeID, parentNodeID} — the Rust tools map their NotAMesh / MeshAlreadyTied / NodeNotFound
/// errors from these fields); `parentNodeID` is OMITTED when negative (the C's `>= 0` guard).
fn tie_err_node(
    code: i64,
    name: &'static str,
    reason: &str,
    node_id: i32,
    parent_node_id: i32,
) -> OpErr {
    OpErr::with_data(code, name, |db| {
        db.add(b"{\"reason\":");
        db.addjson(reason.as_bytes());
        db.add(b",\"nodeID\":");
        db.addi(i64::from(node_id));
        if parent_node_id >= 0 {
            db.add(b",\"parentNodeID\":");
            db.addi(i64::from(parent_node_id));
        }
        db.add(b"}");
    })
}

/// `tie_err_undo_group` (`mcp_bridge.c:4016-4028`): the error for a failure that happened AFTER
/// the tie's undo group opened but left it a NO-OP (FIX-B live L3 / review-b M3) — the group
/// was pushed (one undo.undo with this label pops it) but nothing of the caller's intent
/// executed. `maybeExecuted` is deliberately ABSENT: the caller must count the refusal, not
/// mark an edit, while undoGroupOpened/undoLabel tell it one undo restores the depth.
fn tie_err_undo_group(code: i64, name: &'static str, reason: &str, label: &str) -> OpErr {
    OpErr::with_data(code, name, |db| {
        db.add(b"{\"reason\":");
        db.addjson(reason.as_bytes());
        db.add(b",\"undoGroupOpened\":true,\"undoLabel\":");
        db.addjson(label.as_bytes());
        db.add(b"}");
    })
}

// ---------------------------------------------------------------------------------------------
// `edit.batch`'s pure helpers
// ---------------------------------------------------------------------------------------------

/// `batch_undo_label` (`mcp_bridge.c:2468-2471`): the undo-group label of an edit.batch.
/// hammer-mcp's undo_last decrements its unsaved-edit counter for any popped "MCP: batch"
/// group, which is right exactly when the batch created something (the pop reverts that
/// create); a batch of nothing but entity.setKeyValues ops reverts nothing when popped, so it
/// takes the keyvalue label the counter leaves alone. The label describes the whole batch, so
/// it is decided before the group is opened (`batch_has_create`).
///
/// KNOWN LIMIT (review L5, unchanged and deliberately so): the scan reads the ops *listed*,
/// while the run stops at the first refusal, so a raw mixed batch can still be labelled
/// "MCP: batch" and hold only a keyvalue write (protocol-diff-m2.15.md item 16).
pub(crate) fn batch_undo_label(ops: &[u8]) -> &'static CStr {
    if batch_has_create(ops) {
        c"MCP: batch"
    } else {
        c"MCP: set keyvalues"
    }
}

/// `batch_subst_handle` (`mcp_bridge.c:2474-2504`): `"$N"` in a setKeyValues handle resolves to
/// the handle issued by op N. `params` is the NUL-terminated op params (`pbuf`), modified in
/// place exactly as the C's `cpy(params, tmp, cap)` does.
///
/// An out-of-range `$N` (a negative or wrapped index, the C's own signed-overflow territory
/// after ~19 digits) returns without substituting, where the C would index outside its 16-row
/// table — a memory-safety fix on a request the shipped C cannot answer sanely either.
pub(crate) fn batch_subst_handle(params: &mut [u8], handles: &[[u8; 16]], hcount: usize) {
    let Some(h) = json_find_top(params, b"handle") else {
        return;
    };
    if byte_at(params, h) != b'"' || byte_at(params, h + 1) != b'$' {
        return;
    }
    // :2480-2481: the op index after `"$`, decimal, accumulated in i64 with the C's two's
    // complement wrap (minus the signed-overflow UB).
    let mut n: i64 = 0;
    let mut q = h + 2;
    while byte_at(params, q).is_ascii_digit() {
        n = n.wrapping_mul(10).wrapping_add(i64::from(byte_at(params, q) - b'0'));
        q += 1;
    }
    if n < 0 || n as usize >= hcount || handles[n as usize][0] == 0 {
        return; // :2482
    }
    let token = cstr(&handles[n as usize]);
    let mut tmp = vec![0u8; 1536];
    let prefix = h;
    // :2486-2487: the guard on the tmp size, with the C's `slen(q)` = everything from the
    // first digit to the string's NUL.
    let mut rest = q;
    while byte_at(params, rest) != 0 {
        rest += 1;
    }
    let rest_len = rest - q;
    if prefix + 2 + token.len() + rest_len + 4 >= tmp.len() {
        return;
    }
    tmp[..prefix].copy_from_slice(&params[..prefix]);
    let mut o = prefix;
    tmp[o] = b'"';
    o += 1;
    for &byte in token {
        tmp[o] = byte;
        o += 1;
    }
    tmp[o] = b'"';
    o += 1;
    // :2494-2497: skip to the `"$N"` value's closing quote, step past it, then copy the rest
    // of the params text (the members after `handle`).
    let mut rest = q;
    while byte_at(params, rest) != 0 && byte_at(params, rest) != b'"' {
        rest += 1;
    }
    if byte_at(params, rest) == b'"' {
        rest += 1;
    }
    while byte_at(params, rest) != 0 && o + 1 < tmp.len() {
        tmp[o] = byte_at(params, rest);
        o += 1;
        rest += 1;
    }
    tmp[o] = 0;
    // :2503: `cpy(params, tmp, cap)` — tmp is at most 1535 bytes, never cut by `cap` (8192).
    let end = o.min(params.len() - 1);
    params[..end].copy_from_slice(&tmp[..end]);
    params[end] = 0;
}

/// `tie_kv_prescan` (`mcp_bridge.c:4066-4107`): the dry pass over a tie's keyValues object,
/// BEFORE the undo group opens (review-b L2/L3): apply_keyvalues would silently skip the keys
/// after a malformed value (`{"a":}` stops kv_next early and the tie still succeeds), and a
/// classname/origin/angles key would run SetKeyValue AFTER the solid-class gate and turn the
/// new entity into a point class or move it off the first mesh's origin (SetKeyValue matches
/// case-insensitively, so the compare is too).
///
/// `Ok(())` when the object is clean; the C's exact refusals otherwise. An over-long string or
/// an out-of-range number (`over`) stays the apply pass's per-key warning, exactly as on
/// entity.create — the dry pass only refuses what would corrupt the tie itself.
fn tie_kv_prescan(bytes: &[u8], vp: usize) -> Result<(), OpErr> {
    // The C's `static` scratch (`:2206`), heap here: the buffers only carry one key at a time.
    let mut name = vec![0u8; TIE_KV_NAME_CAP];
    let mut sval = vec![0u8; crate::entity_json::KV_VALUE_MAX + 1];
    let mut nval = vec![0u8; TIE_KV_NAME_CAP];
    let mut p = vp + 1;
    for _guard in 0..4096 {
        let before = p; // kv_next provably advances; never hang the drain
        let Some(value) = crate::entity_json::kv_next(bytes, p, &mut name, &mut sval, &mut nval)
        else {
            // :4077-4085: kv_next returns None right after skipping separators, so the stop
            // char is the first non-separator from `before`: '}' ends the object cleanly,
            // anything else is malformed and the apply pass would drop the rest of the keys
            // silently.
            let mut q = before;
            while matches!(byte_at(bytes, q), b' ' | b',' | b'\n' | b'\t' | b'\r') {
                q += 1;
            }
            if byte_at(bytes, q) == b'}' {
                return Ok(());
            }
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "keyValues: malformed object (the parse does not reach '}')",
            ));
        };
        p = value.next;
        // :4087-4099: the key goes in its own field — sb_addjson quotes it, which inside the
        // reason string would break the JSON (seen live, FIX-B round 2).
        if ci_seq(&name, b"classname") || ci_seq(&name, b"origin") || ci_seq(&name, b"angles") {
            return Err(OpErr::with_data(-32602, "InvalidParams", |db| {
                db.add(b"{\"reason\":\"keyValues: classname/origin/angles are refused on a tie (they would reclass or move the new entity)\",\"key\":");
                db.addjson(cstr(&name));
                db.add(b"}");
            }));
        }
        // `over` stays the apply pass's per-key warning (`:4100-4101`).
    }
    // :4103-4106: deeper than the guard bound.
    Err(OpErr::new(-32602, "InvalidParams", "keyValues: malformed object"))
}

// ---------------------------------------------------------------------------------------------
// `tie_reply_body` (`mcp_bridge.c:4432-4482`)
// ---------------------------------------------------------------------------------------------

/// The tie reply body (FIX-B): shared by the full path and the all-alreadyTied no-op, so the
/// two cannot drift. The `doc` tail is appended by the dispatch tail ([`write_finish`]), not
/// here. `handle` is `handle_new(ent)` (`:4441`) — the op issues it right before calling this;
/// the C calls it from inside, but the token is bridge-side state (no game access) and its
/// value is unaffected (protocol-diff-m3-edit.md). `nodeID` is OMITTED when the entity's id is
/// unreadable (review-b L6: entity.create's convention — read_tie treats a missing nodeID as
/// an internal error, never a node -1), and a mesh row whose id is unreadable carries null.
#[allow(clippy::too_many_arguments)] // the C's parameter list, kept 1:1
fn tie_reply_body(
    t: &dyn Edit,
    out: &mut Sb,
    ent: Node,
    handle: &[u8],
    meshes: &[Node],
    at_ids: &[i32],
    rm_ids: &[i32],
    created: bool,
    kvbuf: Option<&[u8]>,
    noop: bool,
) {
    // :4436-4439
    match t.id(ent) {
        Some(id) if id >= 0 => {
            out.add(b"{\"nodeID\":");
            out.addi(i64::from(id));
        }
        _ => out.add(b"{"),
    }
    // :4441-4442
    out.add(b",\"handle\":");
    out.addjson(handle);
    out.add(b",\"class\":\"entity\"");
    // :4444-4447: the classname read, raw bytes into addjson — the C's `sb_addjson(out, cn)`.
    let cn = t.classname(ent, 512);
    out.add(b",\"classname\":");
    out.addjson(&cn);
    // :4448-4449
    out.add(b",\"created\":");
    out.add(if created { b"true" } else { b"false" });
    // :4450-4451
    let eo = t.origin(ent).unwrap_or([0.0; 3]);
    out.add(b",\"origin\":");
    addf3(out, eo);
    // :4452-4463: the meshes — id then origin per row, unchanged by the tie (world-space
    // transforms); an unreadable id is null (review-b L6).
    out.add(b",\"meshes\":[");
    for (i, mesh) in meshes.iter().enumerate() {
        let mid = t.id(*mesh);
        let mo = t.origin(*mesh).unwrap_or([0.0; 3]);
        if i != 0 {
            out.add(b",");
        }
        out.add(b"{\"nodeID\":");
        match mid {
            Some(id) if id >= 0 => out.addi(i64::from(id)),
            _ => out.add(b"null"),
        }
        out.add(b",\"origin\":");
        addf3(out, mo);
        out.add(b"}");
    }
    // :4464-4467
    out.add(b"],\"alreadyTied\":[");
    for (i, id) in at_ids.iter().enumerate() {
        if i != 0 {
            out.add(b",");
        }
        out.addi(i64::from(*id));
    }
    out.add(b"],\"removedEmptyEntities\":[");
    for (i, id) in rm_ids.iter().enumerate() {
        if i != 0 {
            out.add(b",");
        }
        out.addi(i64::from(*id));
    }
    out.add(b"]");
    // :4469-4472: the additive echo (entity.create's keyValues shape), raw.
    if let Some(kv) = kvbuf {
        out.add(b",\"keyValues\":");
        out.add(kv);
    }
    // :4474-4479: the all-alreadyTied no-op opened no group — say so the way the Rust tool
    // reads it (noop:true, undoLabel:null), so it neither counts an undo group nor an unsaved
    // edit.
    if noop {
        out.add(b",\"noop\":true,\"undoLabel\":null");
    } else {
        out.add(b",\"noop\":false,\"undoLabel\":\"MCP: tie meshes to entity\"");
    }
    out.add(b"}");
}

// ---------------------------------------------------------------------------------------------
// `tie_ancestors_ok` (`mcp_bridge.c:4044-4063`)
// ---------------------------------------------------------------------------------------------

/// `tie_ancestors_ok` (`mcp_bridge.c:4044-4063`): is it safe to reparent `node`? AddChild moves
/// nodes between containers with no file check, and geo_find DOES descend into
/// CMapPrefab/CMapInstance children, so a nodeIDs entry picked from node.list can name a node
/// owned by a referenced sub-map; moving it into a host-map entity would desync the prefab's
/// document and the host map (Hammer's UI never does this — instance contents are not
/// individually selectable). Up via `m_pParent` (mem_ok each step, bounded at 64 like
/// geo_walk), every ancestor must be the document's world root (compared by pointer, so a
/// sub-map's own CMapWorld does not pass), a CMapGroup, or a CMapEntity. A parent that reads as
/// null stops the walk as OK — the same under-world status quo the tie gate already gives such
/// a node; a prefab/instance ON the chain is the positive danger this refuses.
fn tie_ancestors_ok(t: &dyn Edit, doc: Doc, node: Node) -> bool {
    let mut n = node;
    let world = t.world(doc); // :4046: geo_world(doc)
    for _depth in 0..64 {
        let Some(par) = t.parent(n) else {
            return true; // :4047-4048: a null parent stops the walk as OK
        };
        if Some(par) == world {
            return true; // :4049-4050
        }
        // :4051-4055: a CMapGroup or a CMapEntity passes.
        if t.class(par) == "group" || t.class(par) == "entity" {
            n = par;
            continue;
        }
        return false; // :4057: prefab / instance / anything else — refuse
    }
    false // :4060: deeper than the walk bound — refuse, not guess
}

// ---------------------------------------------------------------------------------------------
// `op_entity_tie_meshes` (`mcp_bridge.c:4113-4431`)
// ---------------------------------------------------------------------------------------------

/// `op_entity_tie_meshes` (`mcp_bridge.c:4113-4431`): the whole validation, the mesh gates, the
/// one group, the reparent. Every refusal below `undo_open` carries the C's
/// `undoGroupOpened`/`maybeExecuted` data.
pub(crate) fn op_entity_tie_meshes(
    w: &dyn Write,
    t: &dyn Edit,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let bytes = params.map(str::as_bytes);
    // :4117-4122: exactly one of class / entityID (plan-triggers.md §5); the class gate itself
    // (must be a @SolidClass) lives in the Rust FGD — the bridge re-checks after the create.
    let mut cls = [0u8; 128];
    let mut entity_id: i64 = -1;
    let has_class = bytes.is_some_and(|b| json_find_top(b, b"class").is_some());
    let has_entity = bytes.is_some_and(|b| json_find_top(b, b"entityID").is_some());
    if has_class {
        let region = bytes.unwrap();
        // :4121-4124
        if !json_string_top(region, b"class", &mut cls) || cls[0] == 0 {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "class: a non-empty string is required",
            ));
        }
        // :4126-4129: json_string_at cuts silently, so anything past 63 bytes must be refused
        // before it can masquerade as a shorter class (the cap the wire contract names).
        if cls[63] != 0 {
            return Err(OpErr::new(-32602, "InvalidParams", "class: at most 63 bytes"));
        }
    }
    if has_entity {
        // :4133-4136
        match json_int_top(bytes.unwrap(), b"entityID") {
            Some(value) if (0..=NODEID_MAX).contains(&value) => entity_id = value,
            _ => {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "entityID: a non-negative int up to 2147483647 is required",
                ))
            }
        }
    }
    if has_class == has_entity {
        // :4138-4140
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "exactly one of class / entityID is required",
        ));
    }
    // :4142-4147: keyValues — the entity.create shape, and only with class (a retie target
    // keeps its own).
    let kv = bytes.and_then(|b| json_find_top(b, b"keyValues"));
    if let Some(kv_at) = kv {
        if !has_class || byte_at(bytes.unwrap(), kv_at) != b'{' {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "keyValues: an object, and only with class",
            ));
        }
    }
    // :4149-4160: allowRetie — top-level only and exactly true/false (json_bool would take the
    // first match anywhere).
    let mut allow_retie = false;
    if let Some(arp) = bytes.and_then(|b| json_find_top(b, b"allowRetie")) {
        let region = bytes.unwrap();
        if lit_at(region, arp, b"true") {
            allow_retie = true;
        } else if !lit_at(region, arp, b"false") {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "allowRetie: true or false required",
            ));
        }
    }
    if let Some(kv_at) = kv {
        // :4162-4166 (FIX-B, review-b L2/L3): the whole object is validated BEFORE the group
        // opens.
        tie_kv_prescan(bytes.unwrap(), kv_at)?;
    }
    // :4168-4173: nodeIDs — 1..64 distinct ints (the same reader node.delete uses; an id
    // outside the i32 range would alias another node through the (int) cast).
    let Some(np) = bytes.and_then(|b| json_find_top(b, b"nodeIDs")) else {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeIDs: [int,...] required",
        ));
    };
    let region = &bytes.unwrap()[np..];
    if byte_at(region, 0) != b'[' {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeIDs: [int,...] required",
        ));
    }
    let mut ids = vec![0i32; TIE_MAX_MESHES];
    let mut n = 0usize;
    let mut p = jws(region, 1);
    loop {
        if byte_at(region, p) == b']' {
            break;
        }
        // :4176-4181
        let Some((next, d)) = jnum(region, p) else {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "nodeIDs: non-negative ints up to 2147483647",
            ));
        };
        p = next;
        // The C's `d < 0 || d > 2147483647.0 || d != (double)(int)d` — `d as i32` saturates in
        // Rust where the C's cast is UB; for every d the range test already refused, the
        // equality test never decides the answer, so no byte moves.
        if !(0.0..=2_147_483_647.0).contains(&d) || d != f64::from(d as i32) {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "nodeIDs: non-negative ints up to 2147483647",
            ));
        }
        // :4182-4185
        if n >= TIE_MAX_MESHES {
            return Err(OpErr::new(-32602, "InvalidParams", "nodeIDs: at most 64"));
        }
        ids[n] = d as i32;
        n += 1;
        // :4187-4192
        p = jws(region, p);
        if byte_at(region, p) == b',' {
            p = jws(region, p + 1);
        } else if byte_at(region, p) != b']' {
            return Err(OpErr::new(-32602, "InvalidParams", "nodeIDs: malformed"));
        }
    }
    // :4194-4197
    if n == 0 {
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeIDs: at least one nodeID is required",
        ));
    }
    // :4199-4207
    for i in 0..n {
        for j in i + 1..n {
            if ids[i] == ids[j] {
                return Err(OpErr::new(-32602, "InvalidParams", "nodeIDs: duplicate nodeID"));
            }
        }
    }
    // :4209-4237: the target entity (entityID form) — a live CMapEntity whose FGD class is a
    // solid class, not locked; a point entity or a mesh id is refused before anything is opened.
    let mut existing: Option<Node> = None;
    if has_entity {
        let Some(found) = geo_find(t, doc, entity_id as i32) else {
            // :4216-4217
            return Err(OpErr::new(
                -32022,
                "NodeNotFound",
                "no node with this entityID in the live document",
            ));
        };
        // :4219-4221
        if !w.is_entity(found) || t.gd_type(found) != 5 {
            return Err(OpErr::new(-32602, "InvalidParams", "not a brush entity"));
        }
        // :4223-4225
        if w.node_locked(found) {
            return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked"));
        }
        // :4227-4230 (review-b M5): a target inside a prefab/instance would pull the meshes
        // into the sub-map's document — the same ancestor walk the meshes get below.
        if !tie_ancestors_ok(t, doc, found) {
            return Err(tie_err_node(
                -32602,
                "InvalidParams",
                "entity is inside a prefab/instance",
                entity_id as i32,
                -1,
            ));
        }
        existing = Some(found);
    } else {
        // :4239-4248: the class must resolve in the node factory before the group opens (the
        // same lookup entity.create validates with; CreateNodeByName re-resolves it). Same
        // error as entity.create (review-b M4: the mock and the Rust tool map -32006
        // UnknownClass; a plain -32602 was mock/bridge drift).
        if !w.class_exists(cstr(&cls)) {
            return Err(OpErr::new(
                -32006,
                "UnknownClass",
                "not in the CMapNodeFactory registry",
            ));
        }
    }
    // :4250-4299: every mesh — found, a CMapMesh (refuse groups/entities/props — Hammer's
    // flatten and "Tie Static Objects" paths are out of scope), not locked, and not already
    // tied to ANOTHER entity unless allowRetie (a mesh under the target itself is skipped and
    // reported as alreadyTied, exactly what Hammer skips). A mesh under a group is fine:
    // AddChild moves it between the layer containers.
    let mut meshes = vec![Node(0); TIE_MAX_MESHES];
    let mut skip = [0u8; TIE_MAX_MESHES];
    let mut at_ids = vec![0i32; TIE_MAX_MESHES];
    let mut at_n = 0usize;
    for i in 0..n {
        let Some(nd) = geo_find(t, doc, ids[i]) else {
            // :4257-4258
            return Err(tie_err_node(
                -32022,
                "NodeNotFound",
                "no node with this nodeID in the live document",
                ids[i],
                -1,
            ));
        };
        // :4260-4261 (`is_mesh`, `:3326`)
        if t.class(nd) != "mesh" {
            return Err(tie_err_node(-32602, "InvalidParams", "not a mesh", ids[i], -1));
        }
        // :4263-4266 (review-b M5): a mesh inside a prefab/instance must never be reparented
        // into a host-map entity.
        if !tie_ancestors_ok(t, doc, nd) {
            return Err(tie_err_node(
                -32602,
                "InvalidParams",
                "mesh is inside a prefab/instance",
                ids[i],
                -1,
            ));
        }
        // :4268-4270
        if w.node_locked(nd) {
            return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked"));
        }
        meshes[i] = nd;
        skip[i] = 0;
        // :4272-4298
        if let Some(par) = t.parent(nd) {
            if w.is_entity(par) {
                // :4274-4278: a mesh under the target itself is skipped and reported as
                // alreadyTied, exactly what Hammer skips.
                if existing == Some(par) {
                    skip[i] = 1;
                    at_ids[at_n] = ids[i];
                    at_n += 1;
                    continue;
                }
                // :4280-4285
                if !allow_retie {
                    let pid = t.id(par).unwrap_or(-1);
                    return Err(tie_err_node(-32602, "InvalidParams", "already tied", ids[i], pid));
                }
                // :4287-4296 (review-b M2): a retie EMPTIES the old parent and DeleteNodes it —
                // node.delete refuses locked nodes, so the retie must not bypass the lock on
                // the entity it destroys. The error names the OLD parent ({nodeID: oldParent}).
                if w.node_locked(par) {
                    let pid = t.id(par).unwrap_or(-1);
                    return Err(tie_err_node(
                        -32018,
                        "DocumentNotEditable",
                        "node is locked",
                        pid,
                        -1,
                    ));
                }
            }
        }
    }
    // :4300-4306 (review-b L5): an unreadable origin/angles used to create the entity at
    // [0,0,0] with a normal-looking reply — refuse before the group opens instead. The C's
    // short-circuit order: origin first, angles only when origin read.
    if existing.is_none() && (w.origin(meshes[0]).is_none() || w.angles(meshes[0]).is_none()) {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "the first mesh's origin/angles are unreadable",
        ));
    }
    // :4312-4319 (FIX-B live L1): a request whose EVERY mesh is already under the target
    // entity has nothing to reparent — reply success WITHOUT opening a group (the old code
    // pushed an empty "MCP: tie meshes to entity" group and marked the doc modified, so one
    // undo_last popped nothing visible). at_n counts only entityID-form skips, so at_n == n
    // implies the entityID form; the doc tail the dispatch appends reports the unchanged
    // depth/serial.
    if at_n == n {
        let existing = existing.expect("at_n == n implies the entityID form");
        let handle = hammer::handle_new(existing.0);
        tie_reply_body(
            t,
            out,
            existing,
            handle.as_bytes(),
            &meshes[..n],
            &at_ids[..at_n],
            &[],
            false,
            None,
            true,
        );
        return Ok(());
    }
    // ---- the group (:4325): nothing above opened anything; everything below is undoable ----
    let hist = w.undo_open(doc, TIE_LABEL); // :4336
    let mut vec: Vec<Node> = Vec::with_capacity(TIE_MAX_MESHES + 1);
    let ent: Node;
    if let Some(existing) = existing {
        ent = existing;
        vec.push(ent); // :4338: Hammer records the selected entity too
    } else {
        // :4340-4343: Hammer uses the FIRST node's origin/angles; the reads passed the L5 gate.
        let mut target = CreateTarget {
            cls: cstr(&cls).to_vec(),
            origin: w.origin(meshes[0]).unwrap_or([0.0; 3]),
            angles: w.angles(meshes[0]).unwrap_or([0.0; 3]),
            scales: [0.0; 3],
            has_scales: false,
        };
        let Some(created_node) = w.create_node(doc, &mut target) else {
            // :4346-4350 (FIX-B live L3, review-b M3): the group IS open (undo_open ran
            // above) — the error says so instead of a plain op_err, so the client knows one
            // undo restores the depth. Nothing executed, so no maybeExecuted.
            return Err(tie_err_undo_group(
                -32603,
                "InternalError",
                "CreateNodeByName failed (see bridge log)",
                TIE_LABEL.to_str().unwrap(),
            ));
        };
        ent = created_node;
        // :4353-4362: the defensive solid-class check — the FGD gate is Rust-side, so a point
        // class that was sent anyway is caught here — the created node is deleted in the same
        // group (the group is then a no-op: undoDepth + 1, document content unchanged). The
        // error carries undoGroupOpened/undoLabel (FIX-B live L3, review-b M3).
        if t.gd_type(ent) != 5 {
            w.delete_node(doc, ent);
            return Err(tie_err_undo_group(
                -32602,
                "InvalidParams",
                "not a solid (brush) class",
                TIE_LABEL.to_str().unwrap(),
            ));
        }
    }
    let created = existing.is_none();
    // :4360-4374: keyValues through the entity.create SetKeyValue path (no extra
    // RecordNodeTree: the created node's CreateNodeByName record covers this group —
    // re-props.md Q1). The echo is additive in the reply so a refused key is never silent; an
    // echo that would not fit is dropped loudly (the write itself ran — the keys are on the
    // entity) instead of emitting a cut JSON fragment into the reply.
    let mut kvbuf = vec![0u8; 8192];
    let mut kv_ovf = false;
    if let Some(kv_at) = kv {
        if created {
            if let Some(element) = w.properties(ent) {
                let mut kb = Sb::new(&mut kvbuf);
                let apply = hammer::apply_keyvalues_with(
                    w,
                    ent,
                    element,
                    bytes.unwrap(),
                    kv_at,
                    false, // prev = 0, as the C's `apply_keyvalues(ent, child, kv, &kb, 0)`
                );
                let aux = hammer::kv_apply_into(&mut kb, &apply, false);
                // The C's `kv_ovf = kb.ovf` covers both a full echo and a full scratch builder
                // (`out->ovf = 1`, `:2417`); the port's scratch overflow comes back as `aux`.
                kv_ovf = aux || kb.ovf();
                if kv_ovf {
                    log::line("tie: keyvalues echo too large; the writes ran, the echo is dropped"); // :4366
                }
            } else {
                log::line("tie: entity_properties missing; keyvalues not written"); // :4372
            }
        }
    }
    for mesh in &meshes[..n] {
        vec.push(*mesh);
    }
    // :4378-4380: BEFORE the AddChild calls (Hammer order) — this is what makes the reparent
    // undoable.
    t.record_nodes_and_parents(hist, &vec);
    let mut rm_ids = vec![0i32; TIE_MAX_MESHES];
    let mut tied_ids = vec![0i32; TIE_MAX_MESHES];
    let mut rm_n = 0usize;
    let mut tied_n = 0usize;
    for i in 0..n {
        // :4381-4382: already under the target — Hammer skips it.
        if skip[i] != 0 {
            continue;
        }
        // :4383-4385
        let old = t.parent(meshes[i]);
        if old == Some(ent) {
            continue;
        }
        // :4386-4387: the AddChild + the read-back
        let added = t.add_child(ent, meshes[i]);
        let now = t.parent(meshes[i]);
        if added == 0 && now != Some(ent) {
            // :4389-4421 (review-b M6): the ties before this one already ran and an old entity
            // may already be deleted, so the document WAS modified: DocSetModified now (the
            // old path returned without it, so closing the map might not prompt to save), and
            // the error reports the progress with maybeExecuted:true (the removeConnections
            // contract — verify with entity.get, do not retry). The id lists are capped at
            // TIE_ERR_LIST_CAP entries each (e->data is a fixed buffer).
            w.doc_set_modified(doc);
            return Err(OpErr::with_data(-32603, "InternalError", |db| {
                db.add(b"{\"reason\":\"AddChild refused a mesh; the ties before it in this group already ran - verify with entity.get, do not retry\"");
                db.add(b",\"nodeID\":");
                db.addi(i64::from(ids[i]));
                db.add(b",\"maybeExecuted\":true,\"tied\":[");
                for (k, id) in tied_ids.iter().take(tied_n.min(TIE_ERR_LIST_CAP)).enumerate() {
                    if k != 0 {
                        db.add(b",");
                    }
                    db.addi(i64::from(*id));
                }
                db.add(b"],\"removedEmptyEntities\":[");
                for (k, id) in rm_ids.iter().take(rm_n.min(TIE_ERR_LIST_CAP)).enumerate() {
                    if k != 0 {
                        db.add(b",");
                    }
                    db.addi(i64::from(*id));
                }
                db.add(b"]}");
            }));
        }
        // :4423-4424
        if tied_n < TIE_MAX_MESHES {
            tied_ids[tied_n] = ids[i];
            tied_n += 1;
        }
        // :4426-4431: the emptied-old-brush-entity rule — an old parent that is a brush entity
        // and now has no children is deleted in the same group (Hammer: RemoveNode(old, 3)).
        if let Some(old) = old {
            if w.is_entity(old) && t.child_count(old) == 0 && t.gd_type(old) == 5 {
                let oid = t.id(old).unwrap_or(-1);
                // :4417 (review-b L6): an unreadable id is not reported as -1.
                if oid >= 0 && rm_n < TIE_MAX_MESHES {
                    rm_ids[rm_n] = oid;
                    rm_n += 1;
                }
                w.delete_node(doc, old);
            }
        }
    }
    // :4433
    w.doc_set_modified(doc);
    // :4435-4437
    let handle = hammer::handle_new(ent.0);
    // The C's `(kvbuf[0] && !kv_ovf) ? kvbuf : 0`: an empty (never written) echo is not
    // emitted, only a written one.
    let echo = if !kvbuf.is_empty() && kvbuf[0] != 0 && !kv_ovf {
        Some(cstr(&kvbuf))
    } else {
        None
    };
    tie_reply_body(
        t,
        out,
        ent,
        handle.as_bytes(),
        &meshes[..n],
        &at_ids[..at_n],
        &rm_ids[..rm_n],
        created,
        echo,
        false,
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `op_entity_untie` (`mcp_bridge.c:4485-4564`)
// ---------------------------------------------------------------------------------------------

/// `op_entity_untie` (`mcp_bridge.c:4485-4564`): Hammer's own `CMapDoc::MoveToWorld` with a
/// 1-entry vector — it opens its OWN undo group ("Untie from entity" — undo.undo reports that
/// label), records recursively, moves every child to the entity's parent (the world, or a group
/// the entity was in) and deletes the entity. So: NO undo_open here (a second group would stack
/// an empty one on top), the children are collected BEFORE the call (the entity is gone
/// afterwards), and the entity is never touched again. A side effect is Hammer selecting the
/// untied children — UI state, harmless.
pub(crate) fn op_entity_untie(
    w: &dyn Write,
    t: &dyn Edit,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let bytes = params.map(str::as_bytes);
    // :4489-4492
    let Some(_np) = bytes.and_then(|b| json_find_top(b, b"nodeID")) else {
        return Err(OpErr::new(-32602, "InvalidParams", "nodeID required"));
    };
    // :4493-4496
    let id: i64 = match json_int_top(bytes.unwrap(), b"nodeID") {
        Some(value) if (0..=NODEID_MAX).contains(&value) => value,
        _ => {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "nodeID: a non-negative int up to 2147483647 is required",
            ))
        }
    };
    // :4498: -32022 not found, -32602 not an entity
    let node = geo_entity_node(t, doc, id)?;
    // :4499-4501: a brush entity only — a point entity has no tied meshes, and neither has an
    // entity without children (plan-triggers.md §5)
    if t.gd_type(node) != 5 {
        return Err(OpErr::new(-32602, "InvalidParams", "not a brush entity"));
    }
    // :4503-4505
    if w.node_locked(node) {
        return Err(OpErr::new(-32018, "DocumentNotEditable", "node is locked"));
    }
    // :4507-4514
    let cc = t.child_count(node);
    if cc == 0 {
        return Err(OpErr::new(-32602, "InvalidParams", "not a brush entity"));
    }
    // :4516-4520 (review-b L7): only a CORRUPT count is refused as unreadable; a valid entity
    // with more children than the reply lists is not an internal fault (Hammer itself has no
    // cap) — the reply lists the first CHILDREN_LIST_MAX ids and reports meshesTruncated.
    if !(0..=1_000_000).contains(&cc) {
        return Err(OpErr::new(-32603, "InternalError", "child list unreadable"));
    }
    // :4521-4524: the child handles (`CMapNode_ChildHandles`), read like `geo_walk`'s guard
    // block — the same reads [`hammer::Nodes::children`] does, so the port reuses them.
    let Some(children) = w.children(node) else {
        return Err(OpErr::new(-32603, "InternalError", "child list unreadable"));
    };
    // :4526-4536: the children that go back to the entity's parent, for the reply — collected
    // BEFORE the call, non-map-node handles skipped like geo_walk skips them; an unreadable id
    // is skipped entirely, never reported as -1 (review-b L6).
    let mut kids = vec![0i32; CHILDREN_LIST_MAX];
    let mut kn = 0usize;
    let mut kid_trunc = false;
    for child in children {
        // :4532-4534: an unreadable id and a negative one are both skipped entirely, never
        // reported as -1 (review-b L6; the C's `if (cid < 0) continue;`).
        let Some(cid) = w.id(child).filter(|c| *c >= 0) else {
            continue;
        };
        // :4534-4535
        if kn >= CHILDREN_LIST_MAX {
            kid_trunc = true;
            continue;
        }
        kids[kn] = cid;
        kn += 1;
    }
    // :4539-4542 (review-b L8, defensive): per the RE, MoveToWorld calls entParent->vfn[57]
    // without a check — a node whose parent field is unreadable is refused instead of
    // dereferenced.
    if t.parent(node).is_none() {
        return Err(OpErr::new(-32603, "InternalError", "entity has no parent"));
    }
    // :4545-4547: the C's `CMapNode *one[1] = {node}; NodeVec v = {1, 0, one, 1, EXTERNAL};`
    t.doc_move_to_world(doc, node);
    // :4549
    w.doc_set_modified(doc);
    // :4551-4559
    out.add(b"{\"nodeID\":");
    out.addi(id);
    out.add(b",\"meshes\":[");
    for (i, kid) in kids[..kn].iter().enumerate() {
        if i != 0 {
            out.add(b",");
        }
        out.addi(i64::from(*kid));
    }
    out.add(b"],\"entityDeleted\":true");
    if kid_trunc {
        out.add(b",\"meshesTruncated\":true");
    }
    out.add(b",\"undoLabel\":\"Untie from entity\"}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// `edit.batch` (`mcp_bridge.c:5855-5970`)
// ---------------------------------------------------------------------------------------------

/// The op's params text: the C hands `pbuf` (a C string) to the op; this boundary converts the
/// (possibly op-cut) bytes to the `&str`-shaped op signatures. The lossy replacement is one
/// U+FFFD per maximal invalid subpart — the same one [`Sb::addjson`] emits for the echo, so no
/// echo byte moves (protocol-diff-m3-edit.md).
fn pbuf_text(pbuf: &[u8]) -> String {
    String::from_utf8_lossy(cstr(pbuf)).into_owned()
}

/// The `edit.batch` arm's body after the gate (`mcp_bridge.c:5887-6005`).
fn edit_batch_with(w: &dyn Write, req: &Request) -> String {
    let params = req.params.as_deref();
    let bytes = params.map(str::as_bytes);
    // :5856-5877: the write gate's three checks, each with the C's `{"reason":..}` data — the
    // same order and reason strings `write_gate`'s `OpErr`s answer.
    let doc = match hammer::write_gate(w, bytes) {
        Ok(doc) => doc,
        Err(error) => return error_reply(&req.id, &error),
    };
    // :5879-5885: the ops member
    let ops_at = bytes.and_then(|b| json_find_top(b, b"ops"));
    let ops_open = match (bytes, ops_at) {
        (Some(b), Some(ops_pos)) => byte_at(b, ops_pos) == b'[',
        _ => false,
    };
    if !ops_open {
        // :5883-5885: a bare error, no data.
        return control::error(Some(&req.id), -32602, "invalid params: ops");
    }
    let b = bytes.unwrap();
    let ops = &b[ops_at.unwrap()..];
    // :5887: one undo group for the whole batch (protocol §9)
    w.undo_open(doc, batch_undo_label(ops));
    // :5888-5892: the C's statics, heap here (the buffers are rewritten on every call on the
    // main thread only; the caps are the C's, so the overflow points do not move).
    let mut results = vec![0u8; 256 * 1024]; // :5889: <= 64 entity.create ops, <= 32 keys each
    let mut rb = Sb::new(&mut results);
    rb.add(b"[");
    let mut opbuf = vec![0u8; 8192];
    let mut pbuf = vec![0u8; 8192];
    let mut rbuf = vec![0u8; 65536]; // op params are truncated at pbuf
    let mut handles = [[0u8; 16]; 16];
    let mut hcount = 0usize;
    let mut failed: i64 = -1;
    let mut idx = 0usize;
    let mut first = true;
    let mut batch_ovf = false;
    // :5893-5896: the nodes this group has already recorded (review W2 #7) — a created node's
    // id (from its create reply) and every node a setKeyValues op wrote.
    let mut recorded = vec![0i32; 32];
    let mut rcount = 0usize;
    let mut p = 1usize; // the caller skips the opening '['
    loop {
        // :5900-5903: batch_op_next advances or returns 0; never spin
        let before = p;
        let step = batch_op_next(ops, p, &mut opbuf, &mut pbuf);
        p = step.next;
        if !step.copied {
            break;
        }
        if p == before {
            log::line("edit.batch: op splitter stalled");
            break;
        }
        // :5905-5906: the op's method
        let mut method = [0u8; 64];
        json_string(&opbuf, b"method", &mut method);
        // :5910: the `"$N"` substitution happens BEFORE the rec decision and the dispatch
        batch_subst_handle(&mut pbuf, &handles, hcount);
        // :5912-5913: fresh per op, over the same 65536 scratch
        let mut ob = Sb::new(&mut rbuf);
        let mut error: Option<OpErr> = None;
        let ok: bool;
        // :5917-5951: the per-op dispatch. Both ops call the C's own `op_*` bodies, which open
        // NO undo group: the batch's single group above (`:5878`) is the only one.
        if seq(&method, b"entity.create") {
            match hammer::op_entity_create_no_group(w, doc, Some(&pbuf_text(&pbuf)), &mut ob) {
                Ok(aux) => {
                    if aux {
                        batch_ovf = true;
                    }
                    ok = true;
                }
                Err(failure) => {
                    error = Some(failure);
                    ok = false;
                }
            }
        } else if seq(&method, b"entity.setKeyValues") {
            // :5919-5931 (m2.18, review W2 #7): a node this group already recorded - one the
            // batch created (its CreateNodeByName recorded it in this group, re-props.md Q1)
            // or one an earlier setKeyValues op of this batch wrote - must not RecordNodeTree
            // again: Hammer records each node once per group and a second record's effect on
            // the undo is unverified. Any other node records once, which is what makes the
            // group a real value undo.
            let mut rec = true;
            let mut kv_id: i32 = -1; // the node this op writes, when it resolves one
            let mut hb = [0u8; 16];
            let region = cstr(&pbuf);
            if json_string_top(region, b"handle", &mut hb) && hb[0] != 0 {
                // :5924-5926: the handle resolves to the node; a refused lookup leaves
                // kv_id -1 (rec stays 1).
                if let Ok(hn) = hammer::handle_lookup(&hb) {
                    if let Some(id) = w.id(hn) {
                        kv_id = id;
                    }
                }
            } else if let Some(nid) = json_int_top(region, b"nodeID") {
                // :5928-5930: the nodeID form
                kv_id = nid as i32;
            }
            if kv_id >= 0 {
                // :5931
                rec = batch_record_decision(&mut recorded, &mut rcount, 32, kv_id);
            }
            // :5932
            match hammer::op_set_keyvalues_no_group(
                w,
                doc,
                Some(&pbuf_text(&pbuf)),
                &mut ob,
                rec,
            ) {
                Ok(aux) => {
                    // the C's `apply_keyvalues` sets `out->ovf = 1` on a scratch overflow; the
                    // port reports it as the apply's `aux` and the batch's `ob.ovf` is set at
                    // the same place the C sets it.
                    if aux {
                        batch_ovf = true;
                    }
                    ok = true;
                }
                Err(failure) => {
                    error = Some(failure);
                    ok = false;
                }
            }
        } else {
            // :5951: an unknown method inside the batch
            error = Some(OpErr::new(
                -32011,
                "NotImplemented",
                &String::from_utf8_lossy(cstr(&method)),
            ));
            ok = false;
        }
        // :5953-5954
        if !first {
            rb.add(b",");
        }
        first = false;
        if !ok {
            // :5955-5963: the first failure ends the run; the row is the error element.
            let error = error.expect("a failed op set the error");
            failed = idx as i64;
            rb.add(b"{\"error\":{\"code\":");
            rb.addi(error.code);
            rb.add(b",\"message\":");
            rb.addjson(error.name.as_bytes());
            rb.add(b",\"data\":");
            rb.add(if error.data.is_empty() {
                b"{}"
            } else {
                error.data.as_bytes()
            });
            rb.add(b"}}");
            break;
        }
        // :5964-5966
        if ob.ovf() {
            batch_ovf = true;
        }
        rb.add(ob.as_cstr());
        // :5968-5974: the create bookkeeping — the handle table and the recorded list.
        if seq(&method, b"entity.create") {
            let mut htok = [0u8; 16];
            if json_string(ob.as_cstr(), b"handle", &mut htok) && hcount < handles.len() {
                // :5970: cpy into the 16-byte row (at most 15 bytes + NUL)
                let token = cstr(&htok);
                let end = token.len().min(handles[0].len() - 1);
                handles[hcount][..end].copy_from_slice(&token[..end]);
                handles[hcount][end] = 0;
                hcount += 1;
            }
            // :5972-5974: the created node is recorded by its own create — later setKeyValues
            // ops on it (by `$N` handle or by this id) must not record it again.
            if let Some(nid2) = json_int_top(ob.as_cstr(), b"nodeID") {
                if rcount < recorded.len() {
                    recorded[rcount] = nid2 as i32;
                    rcount += 1;
                }
            }
        }
        idx += 1; // :5983: only a successful op moves the index
    }
    rb.add(b"]"); // :5985
    if batch_ovf || rb.ovf() {
        // :5986-5990: the ops ran but their results do not fit: say so instead of sending cut
        // JSON.
        control::error_ext(
            Some(&req.id),
            -32012,
            "PartialFailure",
            "{\"failedIndex\":null,\"reason\":\"batch result too large; the ops ran - verify with entity.find, do not retry\"}",
        )
    } else if failed >= 0 {
        // :5991-5997: the partial batch's data carries the rows so far and the failed index.
        let data = format!(
            "{{\"results\":{},\"failedIndex\":{}}}",
            String::from_utf8_lossy(rb.as_cstr()),
            failed
        );
        control::error_ext(Some(&req.id), -32012, "PartialFailure", &data)
    } else {
        // :5998-6005: the whole batch as one result, the document state read inside this
        // dispatch (m2.17: one group, read inside this dispatch).
        let mut result = vec![0u8; 256 * 1024 + 1024];
        let mut rsb = Sb::new(&mut result);
        rsb.add(b"{\"results\":");
        rsb.add(rb.as_cstr());
        rsb.add(b",\"failedIndex\":null,\"doc\":");
        hammer::sb_add_doc_object(&mut rsb, doc);
        rsb.add(b"}");
        control::result_ok(Some(&req.id), &String::from_utf8_lossy(rsb.as_cstr()))
    }
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// This group's bodies, in the C dispatch chain's order (`:5855`, `:5988`, `:5990`).
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("edit.batch", edit_batch),
    ("entity.tieMeshes", entity_tie_meshes),
    ("entity.untie", entity_untie),
];

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `entity.tieMeshes` through `geo_dispatch` with `mutating = 1` (`mcp_bridge.c:5988`): the
/// 512 KB result buffer and the `response too large (use limit)` overflow.
fn entity_tie_meshes(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let t = game_edit();
    let params = req.params.as_deref();
    let outcome = hammer::write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_entity_tie_meshes(w, t, doc, params, &mut out).map(|()| (doc, false)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)", // geo_dispatch, `:5777`
    )
}

/// `entity.untie` through `geo_dispatch` with `mutating = 1` (`mcp_bridge.c:5990`).
fn entity_untie(req: &Request) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let w = game_write();
    let t = game_edit();
    let params = req.params.as_deref();
    let outcome = hammer::write_gate(w, params.map(str::as_bytes))
        .and_then(|doc| op_entity_untie(w, t, doc, params, &mut out).map(|()| (doc, false)));
    write_finish(
        &req.id,
        outcome,
        &mut out,
        "response too large (use limit)", // geo_dispatch, `:5777`
    )
}

/// The `edit.batch` arm (`mcp_bridge.c:5855-5970`).
fn edit_batch(req: &Request) -> String {
    edit_batch_with(game_write(), req)
}

/// The production `Edit` reader on the Windows tools build: the hammer lane's live object
/// (its state is the module's pins, the bound `g_hfn` table and the fingerprint/handle
/// statics).
#[cfg(windows)]
fn game_edit() -> &'static dyn Edit {
    &LIVE
}

/// The absent-platform reader: there is no Hammer module (the tools DLL only ever runs on
/// Windows), so the method gate answers `-32017` before a body starts. Anything that still
/// reaches a read fails loudly instead of inventing an answer.
#[cfg(not(windows))]
fn game_edit() -> &'static dyn Edit {
    &NO_EDIT
}

/// The production reader instance. `hammer::win::LiveHammer` is a stateless unit struct.
#[cfg(windows)]
static LIVE: hammer::win::LiveHammer = hammer::win::LiveHammer;

/// The absent-platform reader.
#[cfg(not(windows))]
struct NoEdit;

#[cfg(not(windows))]
static NO_EDIT: NoEdit = NoEdit;

#[cfg(not(windows))]
impl hammer::Nodes for NoEdit {
    fn children(&self, _node: Node) -> Option<Vec<Node>> {
        no_edit("a Hammer node's children")
    }
    fn class(&self, _node: Node) -> &'static str {
        no_edit("a Hammer node's class")
    }
    fn id(&self, _node: Node) -> Option<i32> {
        no_edit("a Hammer node's id")
    }
    fn origin(&self, _node: Node) -> Option<[f32; 3]> {
        no_edit("a Hammer node's origin")
    }
    fn mesh_slots(&self, _node: Node) -> Option<(i32, i32)> {
        no_edit("a Hammer mesh's component counts")
    }
    fn world(&self, _doc: Doc) -> Option<Node> {
        no_edit("a Hammer document's world node")
    }
}

#[cfg(not(windows))]
impl Edit for NoEdit {
    fn parent(&self, _node: Node) -> Option<Node> {
        no_edit("a Hammer node's parent")
    }
    fn gd_type(&self, _node: Node) -> i32 {
        no_edit("a Hammer entity's FGD class type")
    }
    fn child_count(&self, _node: Node) -> i32 {
        no_edit("a Hammer node's child count")
    }
    fn add_child(&self, _ent: Node, _node: Node) -> u8 {
        no_edit("Hammer's AddChild")
    }
    fn record_nodes_and_parents(&self, _hist: usize, _nodes: &[Node]) {
        no_edit("Hammer's RecordNodesAndParents")
    }
    fn doc_move_to_world(&self, _doc: Doc, _node: Node) {
        no_edit("Hammer's DocMoveToWorld")
    }
    fn classname(&self, _node: Node, _cap: usize) -> Vec<u8> {
        no_edit("a Hammer entity's classname")
    }
}

#[cfg(not(windows))]
fn no_edit(what: &str) -> ! {
    panic!("cs2-bridge: {what} needs the Windows tools build (Hammer is a Windows module)")
}

// ---------------------------------------------------------------------------------------------
// The production `Edit` implementation (Windows tools build only)
// ---------------------------------------------------------------------------------------------

/// The live `Edit` reader: the reads and calls over the same objects the hammer lane's readers
/// walk. Main thread only.
#[cfg(windows)]
mod live {
    use core::ffi::c_void;
    use core::ptr;

    use s2sdk_sys::generated::EXTERNAL_CONST_BUFFER_MARKER;
    use s2sdk_sys::generated::{CMapGameDataNode, CMapNode, CMapNode_slots, CUtlVector_CEntityHandle};

    use super::Edit;
    use crate::ops::hammer::{self, Doc, Node};
    use crate::ops::ident::mem_ok;

    /// The C's `NodeVec` (`mcp_bridge.c:3335`): the stack `CUtlVector<CMapNode *>` shape
    /// RecordNodesAndParents and MoveToWorld read — count at +0, data at +8. `flags` carries
    /// S2_UTLVEC_EXTERNAL (the generated `EXTERNAL_CONST_BUFFER_MARKER`, 1<<30):
    /// "caller-owned, never realloc/free" — with flags 0 a future build that appended to (or
    /// purged) the vector would realloc/free a stack pointer (review-b L1).
    #[repr(C)]
    struct NodeVec {
        count: i32,
        pad: i32,
        data: *mut *mut c_void,
        alloc: i32,
        flags: i32,
    }

    const _: () = {
        assert!(core::mem::offset_of!(NodeVec, data) == 8);
        assert!(core::mem::offset_of!(NodeVec, alloc) == 16);
        assert!(core::mem::offset_of!(NodeVec, flags) == 20);
    };

    /// The `int flags` member the C initializes with `S2_UTLVEC_EXTERNAL` (`:4371`, `:4545`).
    const UTLVEC_EXTERNAL: i32 = EXTERNAL_CONST_BUFFER_MARKER as i32;

    impl Edit for hammer::win::LiveHammer {
        fn parent(&self, node: Node) -> Option<Node> {
            let node = node.0 as *mut CMapNode;
            // SAFETY: `addr_of!` computes the member's address only (no reference into game
            // memory); the caller passed a node a walk reached (`geo_find`/`geo_entity_node`,
            // which identity-check every hop).
            let field = unsafe { ptr::addr_of!((*node).m_pParent) };
            if !mem_ok(field.cast()) {
                return None; // :3354: the field is unreadable
            }
            // SAFETY: the pointer member was just proven readable.
            let parent = unsafe { field.read_unaligned() };
            if parent.is_null() || !mem_ok(parent.cast()) {
                // :3357: `(p && mem_ok(p)) ? p : 0`
                return None;
            }
            Some(Node(parent as usize))
        }

        fn gd_type(&self, node: Node) -> i32 {
            // The C strides the same fields by `S2_OFF_CMapGameDataNode_m_pGameDataClass` /
            // `S2_OFF_FgdClass_t_m_nType` (`bridge/s2sdk_gen.h`); the generated
            // `CMapGameDataNode` / `FgdClass_t` are the same records, so the generated field
            // addresses are those offsets.
            let gd_node = node.0 as *const CMapGameDataNode;
            // SAFETY: `addr_of!` computes the member's address only; every read below is
            // `mem_ok`-guarded exactly like the C's (`:3344-3351`).
            let field = unsafe { ptr::addr_of!((*gd_node).m_pGameDataClass) };
            if !mem_ok(field.cast()) || !mem_ok((field as *const u8).wrapping_add(7).cast()) {
                return 0;
            }
            // SAFETY: the 8-byte pointer member was just proven readable.
            let gd = unsafe { field.read_unaligned() };
            if gd.is_null() || !mem_ok(gd.cast()) {
                return 0;
            }
            // SAFETY: `addr_of!` again; the type's four bytes are probed before the read.
            let ty = unsafe { ptr::addr_of!((*gd).m_nType) };
            if !mem_ok(ty.cast()) || !mem_ok((ty as *const u8).wrapping_add(3).cast()) {
                return 0;
            }
            // SAFETY: the four bytes were just proven readable.
            unsafe { ty.read_unaligned() }
        }

        fn child_count(&self, node: Node) -> i32 {
            let node = node.0 as *mut CMapNode;
            // SAFETY: the same cast the hammer lane's `child_vector` makes; `addr_of!` computes
            // the member's address only.
            let vector = unsafe {
                ptr::addr_of!((*node).m_Children).cast::<CUtlVector_CEntityHandle>()
            };
            // SAFETY: `addr_of!` computes the member's address only, from the node pointer the
            // op identity-checked (a `geo_find` node that passed the entity/mesh class gates).
            let size = unsafe { ptr::addr_of!((*vector).m_Size) };
            // :3361-3362: the count's four bytes must be readable, else -1 (never guess).
            if !mem_ok(size.cast()) || !mem_ok((size as *const u8).wrapping_add(3).cast()) {
                return -1;
            }
            // SAFETY: the four bytes were just proven readable.
            unsafe { size.read_unaligned() }
        }

        fn add_child(&self, ent: Node, node: Node) -> u8 {
            let ent = ent.0 as *mut CMapNode;
            // The C header's own signature (`bridge/s2sdk.h:473`):
            // `u8 (*AddChild)(CMapNode *self, CMapNode *child)`. The generated typed field
            // returns a Rust `bool`, but the C reads `u8 added` and tests it — so the entry is
            // read through the generated slot index (`CMapNode_slots::AddChild`, 57) instead,
            // as `ops/map.rs` `save` does for `Save`.
            type AddChildFn = unsafe extern "C" fn(*mut CMapNode, *mut CMapNode) -> u8;
            // SAFETY: `ent` is a node the op identity-checked (`geo_find` reached it and it
            // passed the entity/mesh class gates), so its vptr at offset 0 is the pinned class
            // vtable; the slot index is a generated constant inside `VTLEN`.
            let vptr = unsafe { ent.cast::<*const *const c_void>().read_unaligned() };
            let entries = vptr.cast::<*const c_void>();
            // SAFETY: the slot index is the generated `CMapNode_slots::AddChild`.
            let entry = unsafe { *entries.add(CMapNode_slots::AddChild) };
            // SAFETY: the bit pattern is a function pointer of the C header's own type.
            let f: AddChildFn = unsafe { core::mem::transmute_copy(&entry) };
            // SAFETY: the verified AddChild slot, called with the recorded entity and the
            // caller's mesh (`:4386`).
            unsafe { f(ent, node.0 as *mut CMapNode) }
        }

        fn record_nodes_and_parents(&self, hist: usize, nodes: &[Node]) {
            let Some(table) = hammer::bind::hfn() else {
                return; // as the hammer lane's reads answer before the bind
            };
            // The C's `NodeVec v = { vc, 0, vec, vc, S2_UTLVEC_EXTERNAL }` (`:4371`): the
            // vector owns the pointers for the call (EXTERNAL never frees it).
            let mut pointers: Vec<*mut c_void> =
                nodes.iter().map(|node| node.0 as *mut c_void).collect();
            let v = NodeVec {
                count: pointers.len() as i32,
                pad: 0,
                data: pointers.as_mut_ptr(),
                alloc: pointers.len() as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: the verified HFN_RECORD_NODES_AND_PARENTS function (`:4378`), called
            // with the group token `undo_open` returned and the C's recursion flag 0;
            // `pointers` outlives the call.
            unsafe {
                (table.record_nodes_and_parents)(
                    hist as *mut c_void,
                    &v as *const NodeVec as *const c_void,
                    0,
                )
            };
        }

        fn doc_move_to_world(&self, doc: Doc, node: Node) {
            let Some(table) = hammer::bind::hfn() else {
                return;
            };
            // The C's `CMapNode *one[1] = {node}; NodeVec v = {1, 0, one, 1, EXTERNAL}`
            // (`:4545`).
            let mut one: [*mut c_void; 1] = [node.0 as *mut c_void];
            let v = NodeVec {
                count: 1,
                pad: 0,
                data: one.as_mut_ptr(),
                alloc: 1,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: the verified HFN_DOC_MOVE_TO_WORLD function (`:4547`), called with the
            // gated document; it opens the move's own undo group.
            unsafe {
                (table.doc_move_to_world)(doc.0 as *mut c_void, &v as *const NodeVec as *const c_void)
            };
        }

        fn classname(&self, node: Node, cap: usize) -> Vec<u8> {
            // `:4445-4447`: `g_hfn.GetEntityProperties(ent)`; the C's non-null + mem_ok guard,
            // then `child_string(props, "classname", sizeof cn)`.
            let Some(element) = hammer::win::entity_properties(node.0 as *mut CMapNode) else {
                return Vec::new();
            };
            hammer::win::child_string(element, c"classname", cap)
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // each case names the one hop that fails
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::time::Instant;

    use super::*;

    // -----------------------------------------------------------------------------------------
    // A fabricated Hammer + Edit: the one-level tree, no game memory. The tie/untie/batch
    // bodies run through it; every game call is recorded so the tests can assert the C's call
    // order and that a refusal touched nothing.
    // -----------------------------------------------------------------------------------------

    #[derive(Clone, Default)]
    struct FakeNode {
        class: &'static str,
        id: Option<i32>,
        origin: Option<[f32; 3]>,
        angles: Option<[f32; 3]>,
        parent: Option<usize>,
        children: Option<Vec<usize>>, // None = unreadable (the C's early return)
        gd: i32,
        properties: Option<usize>,
        classname: Vec<u8>,
        locked: bool,
    }

    #[derive(Default)]
    struct Script {
        class_exists: bool,
        creates: Vec<usize>, // the nodes create_node answers, in order; exhausted = fail
        add_ok: bool,
        land: usize, // add_child lands this many times, then refuses (0 = add_ok's rule)
    }

    #[allow(clippy::type_complexity)] // the recording fake's call logs, test-only
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
        nodes: RefCell<BTreeMap<usize, FakeNode>>,
        script: RefCell<Script>,
        calls: RefCell<Vec<String>>,
        deleted: RefCell<Vec<usize>>,
        modified: RefCell<u32>,
        recorded: RefCell<Vec<(usize, Vec<usize>)>>,
        moved: RefCell<Vec<usize>>,
        undo_labels: RefCell<Vec<Vec<u8>>>,
        set_kvs: RefCell<Vec<(usize, Vec<u8>, Vec<u8>)>>,
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
                nodes: RefCell::new(BTreeMap::new()),
                script: RefCell::new(Script::default()),
                calls: RefCell::new(Vec::new()),
                deleted: RefCell::new(Vec::new()),
                modified: RefCell::new(0),
                recorded: RefCell::new(Vec::new()),
                moved: RefCell::new(Vec::new()),
                undo_labels: RefCell::new(Vec::new()),
                set_kvs: RefCell::new(Vec::new()),
            }
        }
    }

    impl Fake {
        fn with_node(self, handle: usize, node: FakeNode) -> Self {
            self.nodes.borrow_mut().insert(handle, node);
            self
        }

        fn node(&self, handle: usize) -> FakeNode {
            self.nodes.borrow().get(&handle).cloned().unwrap_or_default()
        }

        fn set_node(&self, handle: usize, node: FakeNode) {
            self.nodes.borrow_mut().insert(handle, node);
        }

        fn script<T>(&self, f: impl FnOnce(&Script) -> T) -> T {
            f(&self.script.borrow())
        }

        fn set_script(&self, f: impl FnOnce(&mut Script)) {
            f(&mut self.script.borrow_mut());
        }

        fn script_mut<T>(&self, f: impl FnOnce(&mut Script) -> T) -> T {
            f(&mut self.script.borrow_mut())
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn undo_labels(&self) -> Vec<Vec<u8>> {
            self.undo_labels.borrow().clone()
        }

        fn record(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl hammer::Chain for Fake {
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
            match self.nodes.borrow().get(&object) {
                Some(node) if vtable == pins::HV_MAPENTITY_VT.value as u32 => node.class == "entity",
                Some(node) if vtable == pins::HV_MAPGROUP_VT.value as u32 => node.class == "group",
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

    impl hammer::Nodes for Fake {
        fn children(&self, node: Node) -> Option<Vec<Node>> {
            self.nodes
                .borrow()
                .get(&node.0)?
                .children
                .clone()
                .map(|kids| kids.into_iter().map(Node).collect())
        }
        fn class(&self, node: Node) -> &'static str {
            self.nodes.borrow().get(&node.0).map_or("other", |node| node.class)
        }
        fn id(&self, node: Node) -> Option<i32> {
            self.nodes.borrow().get(&node.0).and_then(|node| node.id)
        }
        fn origin(&self, node: Node) -> Option<[f32; 3]> {
            self.nodes.borrow().get(&node.0).and_then(|node| node.origin)
        }
        fn mesh_slots(&self, _node: Node) -> Option<(i32, i32)> {
            Some((8, 12))
        }
        fn world(&self, _doc: Doc) -> Option<Node> {
            self.world.map(Node)
        }
    }

    impl Write for Fake {
        fn class_exists(&self, cls: &[u8]) -> bool {
            self.record(format!("class_exists {}", String::from_utf8_lossy(cls)));
            self.script(|s| s.class_exists)
        }
        fn create_node(&self, doc: Doc, target: &mut CreateTarget) -> Option<Node> {
            self.record(format!(
                "create doc={} class={} origin=[{},{},{}] angles=[{},{},{}]",
                doc.0,
                String::from_utf8_lossy(&target.cls),
                target.origin[0],
                target.origin[1],
                target.origin[2],
                target.angles[0],
                target.angles[1],
                target.angles[2],
            ));
            self.script_mut::<Option<usize>>(|s| {
                if s.creates.is_empty() {
                    None
                } else {
                    Some(s.creates.remove(0))
                }
            })
            .map(Node)
        }
        fn properties(&self, node: Node) -> Option<usize> {
            self.record(format!("properties {}", node.0));
            self.nodes.borrow().get(&node.0).and_then(|node| node.properties)
        }
        fn undo_open(&self, doc: Doc, label: &'static CStr) -> usize {
            self.record(format!(
                "undo_open doc={} label={}",
                doc.0,
                String::from_utf8_lossy(label.to_bytes())
            ));
            self.undo_labels.borrow_mut().push(label.to_bytes().to_vec());
            self.active_hist
        }
        fn record_node_tree(&self, hist: usize, node: Node) {
            self.record(format!("record_tree hist={hist} node={}", node.0));
        }
        fn doc_set_modified(&self, doc: Doc) {
            self.record(format!("modified doc={}", doc.0));
            *self.modified.borrow_mut() += 1;
        }
        fn get_active_history(&self) -> usize {
            self.record("active_history".to_string());
            self.active_hist
        }
        fn get_redo_history(&self, _doc: Doc) -> usize {
            0
        }
        fn history_ok(&self, _hist: usize) -> bool {
            true
        }
        fn history_info(&self, _hist: usize) -> hammer::HistoryInfo {
            hammer::HistoryInfo::default()
        }
        fn undo_pop(&self, _hist: usize) {}
        fn node_locked(&self, node: Node) -> bool {
            self.record(format!("locked {}", node.0));
            self.nodes.borrow().get(&node.0).is_some_and(|node| node.locked)
        }
        fn delete_node(&self, doc: Doc, node: Node) {
            self.record(format!("delete doc={} node={}", doc.0, node.0));
            self.deleted.borrow_mut().push(node.0);
        }
        fn set_origin(&self, _node: Node, _value: [f32; 3]) {}
        fn set_angles(&self, _node: Node, _value: [f32; 3]) {}
        fn set_scales(&self, _node: Node, _value: [f32; 3]) {}
        fn angles(&self, node: Node) -> Option<[f32; 3]> {
            self.record(format!("angles {}", node.0));
            self.nodes.borrow().get(&node.0).and_then(|node| node.angles)
        }
        fn scales(&self, _node: Node) -> Option<[f32; 3]> {
            Some([1.0; 3])
        }
        fn attr_previous(&self, _element: usize, name: &[u8]) -> hammer::PrevValue {
            self.record(format!("previous {}", String::from_utf8_lossy(name)));
            hammer::PrevValue::Null
        }
        fn attr_info(&self, _element: usize, name: &[u8]) -> hammer::AttrInfo {
            self.record(format!("attr_info {}", String::from_utf8_lossy(name)));
            // A creatable key: interned, absent (the C's SetKeyValue-creates-String path).
            hammer::AttrInfo {
                symbol: true,
                attr: 0,
                present: false,
                readable: false,
                kind: -1,
            }
        }
        fn case_variant(&self, _element: usize, _name: &[u8]) -> Option<Vec<u8>> {
            self.record("variant".to_string());
            None
        }
        fn set_key_value(&self, node: Node, name: &[u8], value: &[u8]) {
            self.record(format!(
                "set_kv node={} {}={}",
                node.0,
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(value),
            ));
            self.set_kvs
                .borrow_mut()
                .push((node.0, name.to_vec(), value.to_vec()));
        }
        fn set_value_int(&self, _attr: usize, _value: i32) {}
        fn set_value_float(&self, _attr: usize, _value: f32) {}
        fn conn_count(&self, _node: Node) -> Option<usize> {
            Some(0)
        }
        fn conn_entry(&self, _node: Node, _index: usize) -> Option<hammer::Conn> {
            None
        }
        fn conn_filter_fields(&self, _node: Node, _index: usize) -> Option<hammer::FilterFields> {
            None
        }
        fn add_connection(
            &self,
            _node: Node,
            _spec: &crate::entity_json::ConnSpec,
        ) -> (bool, Option<i32>) {
            (false, None)
        }
        fn remove_connection(&self, _node: Node, _index: i32) -> bool {
            false
        }
    }

    impl Edit for Fake {
        fn parent(&self, node: Node) -> Option<Node> {
            self.record(format!("parent {}", node.0));
            self.nodes.borrow().get(&node.0).and_then(|node| node.parent).map(Node)
        }
        fn gd_type(&self, node: Node) -> i32 {
            self.record(format!("gd {}", node.0));
            self.nodes.borrow().get(&node.0).map_or(0, |node| node.gd)
        }
        fn child_count(&self, node: Node) -> i32 {
            self.record(format!("cc {}", node.0));
            self.nodes
                .borrow()
                .get(&node.0)
                .and_then(|node| node.children.as_ref().map(|kids| kids.len() as i32))
                .unwrap_or(-1)
        }
        fn add_child(&self, ent: Node, node: Node) -> u8 {
            self.record(format!("add_child ent={} node={}", ent.0, node.0));
            let scripted = self.script(|s| s.land);
            if scripted != 0 {
                // Scripted landing: `land` calls land, then refuse (the cap test).
                let mut script = self.script.borrow_mut();
                if script.land == 0 {
                    return 0;
                }
                script.land -= 1;
                drop(script);
            } else if !self.script(|s| s.add_ok) {
                return 0;
            }
            // The tree behaves like the real one: the child is detached from its old parent
            // and adopted by `ent` (world-space transforms are untouched).
            let mut nodes = self.nodes.borrow_mut();
            let old = nodes.get(&node.0).and_then(|child| child.parent);
            if let Some(old) = old {
                if let Some(parent) = nodes.get_mut(&old) {
                    if let Some(kids) = parent.children.as_mut() {
                        kids.retain(|kid| *kid != node.0);
                    }
                }
            }
            if let Some(child) = nodes.get_mut(&node.0) {
                child.parent = Some(ent.0);
            }
            if let Some(parent) = nodes.get_mut(&ent.0) {
                parent.children.get_or_insert_with(Vec::new).push(node.0);
            }
            1
        }
        fn record_nodes_and_parents(&self, hist: usize, nodes: &[Node]) {
            self.record(format!(
                "record_nodes hist={} nodes={:?}",
                hist,
                nodes.iter().map(|node| node.0).collect::<Vec<_>>()
            ));
            self.recorded
                .borrow_mut()
                .push((hist, nodes.iter().map(|node| node.0).collect()));
        }
        fn doc_move_to_world(&self, doc: Doc, node: Node) {
            self.record(format!("move_to_world doc={} node={}", doc.0, node.0));
            self.moved.borrow_mut().push(node.0);
        }
        fn classname(&self, node: Node, _cap: usize) -> Vec<u8> {
            self.record(format!("classname {}", node.0));
            self.node(node.0).classname
        }
    }

    /// A world with one entity target and one mesh, both under the world.
    fn two_nodes(gd: i32) -> Fake {
        Fake::default()
            .with_node(
                10,
                FakeNode {
                    class: "world",
                    id: None,
                    origin: None,
                    angles: None,
                    parent: None,
                    children: Some(vec![20, 30]),
                    gd: 0,
                    properties: None,
                    classname: Vec::new(),
                    locked: false,
                },
            )
            .with_node(
                20,
                FakeNode {
                    class: "entity",
                    id: Some(7),
                    origin: Some([3.0; 3]),
                    angles: Some([0.0; 3]),
                    parent: Some(10),
                    children: Some(Vec::new()),
                    gd,
                    properties: Some(400),
                    classname: b"trigger_once".to_vec(),
                    locked: false,
                },
            )
            .with_node(
                30,
                FakeNode {
                    class: "mesh",
                    id: Some(12),
                    origin: Some([64.0, 64.0, 0.0]),
                    angles: Some([0.0; 3]),
                    parent: Some(10),
                    children: Some(Vec::new()),
                    gd: 0,
                    properties: None,
                    classname: Vec::new(),
                    locked: false,
                },
            )
    }

    fn run_tie(fake: &Fake, params: &str) -> Result<String, OpErr> {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op_entity_tie_meshes(fake, fake, Doc(4), Some(params), &mut out)?;
        Ok(String::from_utf8_lossy(out.as_cstr()).into_owned())
    }

    fn run_untie(fake: &Fake, params: Option<&str>) -> Result<String, OpErr> {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op_entity_untie(fake, fake, Doc(4), params, &mut out)?;
        Ok(String::from_utf8_lossy(out.as_cstr()).into_owned())
    }

    fn batch_request(params: Option<&str>, id: &str) -> Request {
        Request {
            id: id.to_string(),
            method: "edit.batch".to_string(),
            params: params.map(str::to_string),
            received: Instant::now(),
            deadline: None,
            gen: 1,
        }
    }

    /// The batch arm from the handler's entry down: the gate, the ops member, the loop, the
    /// reply envelope.
    fn run_batch_arm(fake: &Fake, params: &str, id: &str) -> String {
        edit_batch_with(fake, &batch_request(Some(params), id))
    }

    /// The batch's params with the write gate's `expectDoc` (`:1928-1948`) naming the host
    /// fingerprint (`d0`, epoch 0: no game module ever sampled it).
    fn batch_params(ops: &str) -> String {
        format!("{{\"expectDoc\":{{\"id\":\"d0\",\"epoch\":0}},\"ops\":{ops}}}")
    }

    /// The `which`-th `"handle":"nK"` token a reply carries (the token number depends on the
    /// process-wide handle counter, so the expected strings substitute it).
    fn reply_token(reply: &str, which: usize) -> String {
        let mut from = 0;
        for _ in 0..=which {
            let at = reply[from..]
                .find(r#""handle":""#)
                .expect("a create row carries a handle")
                + from;
            from = at + 10;
        }
        let end = reply[from..].find('"').unwrap() + from;
        reply[from..end].to_string()
    }

    // -----------------------------------------------------------------------------------------
    // Param validation (the brief's item 2): every malformed-params path, no game access.
    // -----------------------------------------------------------------------------------------

    /// `op_entity_tie_meshes`'s param gates, in the C's check order
    /// (`bridge/mcp_bridge.c:4117-4248`).
    #[test]
    fn tie_refuses_its_params_in_the_c_order_without_touching_the_game() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (params, code, name, reason)
            ("{}", "-32602", "InvalidParams", "exactly one of class / entityID is required"),
            (r#"{"class":5}"#, "-32602", "InvalidParams", "class: a non-empty string is required"),
            (r#"{"class":""}"#, "-32602", "InvalidParams", "class: a non-empty string is required"),
            (
                r#"{"entityID":-1}"#,
                "-32602",
                "InvalidParams",
                "entityID: a non-negative int up to 2147483647 is required",
            ),
            (
                r#"{"entityID":2147483648}"#,
                "-32602",
                "InvalidParams",
                "entityID: a non-negative int up to 2147483647 is required",
            ),
            (
                r#"{"class":"trigger_once","entityID":7}"#,
                "-32602",
                "InvalidParams",
                "exactly one of class / entityID is required",
            ),
            (
                r#"{"entityID":7,"keyValues":{"a":1}}"#,
                "-32602",
                "InvalidParams",
                "keyValues: an object, and only with class",
            ),
            (
                r#"{"class":"trigger_once","keyValues":"[1]"}"#,
                "-32602",
                "InvalidParams",
                "keyValues: an object, and only with class",
            ),
            (
                r#"{"class":"trigger_once","allowRetie":"yes"}"#,
                "-32602",
                "InvalidParams",
                "allowRetie: true or false required",
            ),
            (
                r#"{"class":"trigger_once","nodeIDs":[]}"#,
                "-32602",
                "InvalidParams",
                "nodeIDs: at least one nodeID is required",
            ),
            (r#"{"class":"trigger_once"}"#, "-32602", "InvalidParams", "nodeIDs: [int,...] required"),
            (
                r#"{"class":"trigger_once","nodeIDs":[1,1]}"#,
                "-32602",
                "InvalidParams",
                "nodeIDs: duplicate nodeID",
            ),
            (
                r#"{"class":"trigger_once","nodeIDs":[-1]}"#,
                "-32602",
                "InvalidParams",
                "nodeIDs: non-negative ints up to 2147483647",
            ),
            (
                r#"{"class":"trigger_once","nodeIDs":[1 2]}"#,
                "-32602",
                "InvalidParams",
                "nodeIDs: malformed",
            ),
        ];
        for (params, code, name, reason) in cases {
            let fake = Fake::default();
            let error = run_tie(&fake, params).expect_err(reason);
            assert_eq!(error.code.to_string(), *code, "{params}: code");
            assert_eq!(error.name, *name, "{params}: name");
            assert_eq!(
                error.data,
                format!("{{\"reason\":\"{reason}\"}}"),
                "{params}: data"
            );
            assert!(
                fake.calls().is_empty(),
                "{params}: the refusal must not touch the game: {:?}",
                fake.calls()
            );
        }
    }

    /// `tie_err_node` / `tie_err_undo_group` (`mcp_bridge.c:3998-4028`): the exact `data`
    /// objects, including the `>= 0` parent guard and the absent `maybeExecuted`.
    #[test]
    fn tie_error_builders_build_the_c_data_objects() {
        assert_eq!(
            tie_err_node(-32022, "NodeNotFound", "no node", 12, -1).data,
            r#"{"reason":"no node","nodeID":12}"#
        );
        assert_eq!(
            tie_err_node(-32602, "InvalidParams", "already tied", 12, 7).data,
            r#"{"reason":"already tied","nodeID":12,"parentNodeID":7}"#
        );
        assert_eq!(
            tie_err_undo_group(-32603, "InternalError", "failed", "MCP: tie meshes to entity").data,
            r#"{"reason":"failed","undoGroupOpened":true,"undoLabel":"MCP: tie meshes to entity"}"#
        );
    }

    /// `:4182-4185`: at most 64 nodeIDs.
    #[test]
    fn tie_refuses_more_than_64_node_ids() {
        let mut params = String::from(r#"{"class":"trigger_once","nodeIDs":["#);
        for i in 0..65 {
            if i != 0 {
                params.push(',');
            }
            params.push_str(&i.to_string());
        }
        params.push_str("]}");
        let fake = Fake::default();
        let error = run_tie(&fake, &params).expect_err("65 ids are refused");
        assert_eq!(error.code, -32602);
        assert_eq!(error.data, r#"{"reason":"nodeIDs: at most 64"}"#);
        assert!(fake.calls().is_empty(), "no game access");
    }

    /// `:4162-4166` (FIX-B): a malformed keyValues object is refused BEFORE the group opens.
    #[test]
    fn tie_prescans_the_keyvalues_object_before_anything_opens() {
        let fake = Fake::default();
        let error = run_tie(&fake, r#"{"class":"trigger_once","keyValues":{"a":"b","c":}}"#)
            .expect_err("malformed object");
        assert_eq!(
            error.data,
            r#"{"reason":"keyValues: malformed object (the parse does not reach '}')"}"#
        );
        assert!(fake.calls().is_empty(), "no game access");
    }

    /// `:4087-4099`: the dangerous keys are refused, and the key goes in its own field so a
    /// quoted key cannot break the reason string (FIX-B round 2).
    #[test]
    fn tie_refuses_the_three_dangerous_keys_with_the_key_field() {
        for key in ["classname", "origin", "angles"] {
            let params = format!(r#"{{"class":"trigger_once","keyValues":{{"{key}":"x"}}}}"#);
            let fake = Fake::default();
            let error = run_tie(&fake, &params).expect_err("the key is refused");
            assert_eq!(
                error.data,
                format!(
                    r#"{{"reason":"keyValues: classname/origin/angles are refused on a tie (they would reclass or move the new entity)","key":"{key}"}}"#
                ),
                "{key}: the C's key refusal data"
            );
            assert!(fake.calls().is_empty(), "no game access");
        }
    }

    /// `:4211-4237`: the entityID form's gates before anything opens.
    #[test]
    fn tie_entity_form_gates() {
        // no node with this entityID (`:4216-4217`)
        let fake = two_nodes(5);
        let error = run_tie(&fake, r#"{"entityID":9,"nodeIDs":[12]}"#).expect_err("not found");
        assert_eq!(
            error.data,
            r#"{"reason":"no node with this entityID in the live document"}"#
        );
        assert_eq!(error.code, -32022);
        assert!(fake.calls().is_empty(), "the walk itself is not a recorded call");

        // a point class (`gd_type == 4`, `:4219-4221`)
        let fake = two_nodes(4);
        let error = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect_err("point class");
        assert_eq!(error.data, r#"{"reason":"not a brush entity"}"#);
        assert_eq!(error.code, -32602);

        // locked (`:4223-4225`)
        let fake = two_nodes(5);
        fake.set_node(
            20,
            FakeNode {
                locked: true,
                ..fake.node(20)
            },
        );
        let error = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect_err("locked");
        assert_eq!(error.data, r#"{"reason":"node is locked"}"#);
        assert_eq!(error.code, -32018);

        // inside a prefab (`:4227-4230`): the entity's parent is a prefab node
        let fake = two_nodes(5);
        fake.set_node(
            20,
            FakeNode {
                parent: Some(50),
                ..fake.node(20)
            },
        );
        fake.set_node(50, FakeNode { class: "prefab", ..Default::default() });
        let error = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect_err("prefab parent");
        assert_eq!(
            error.data,
            r#"{"reason":"entity is inside a prefab/instance","nodeID":7}"#
        );
        assert_eq!(error.code, -32602);
    }

    /// `:4239-4248`: the class must resolve in the factory before the group opens; the mock
    /// maps -32006 UnknownClass (review-b M4).
    #[test]
    fn tie_class_form_needs_the_class_in_the_factory() {
        let fake = Fake::default();
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("no factory entry");
        assert_eq!(error.code, -32006);
        assert_eq!(error.data, r#"{"reason":"not in the CMapNodeFactory registry"}"#);
        assert_eq!(fake.calls(), vec!["class_exists trigger_once"], "only the lookup");
    }

    /// `:4250-4270`: the per-mesh gates.
    #[test]
    fn tie_mesh_gates() {
        let base = || {
            let fake = Fake::default().with_node(
                10,
                FakeNode {
                    class: "world",
                    id: None,
                    origin: None,
                    angles: None,
                    parent: None,
                    children: Some(vec![30]),
                    gd: 0,
                    properties: None,
                    classname: Vec::new(),
                    locked: false,
                },
            );
            // the class form's factory lookup comes before the mesh walk (`:4239-4248`)
            fake.set_script(|s| s.class_exists = true);
            fake
        };
        let mesh_node = |parent: Option<usize>| FakeNode {
            class: "mesh",
            id: Some(12),
            origin: Some([64.0, 64.0, 0.0]),
            angles: Some([0.0; 3]),
            parent,
            children: Some(Vec::new()),
            gd: 0,
            properties: None,
            classname: Vec::new(),
            locked: false,
        };

        // the mesh is not found (`:4257-4258`)
        let fake = base();
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("not found");
        assert_eq!(
            error.data,
            r#"{"reason":"no node with this nodeID in the live document","nodeID":12}"#
        );
        assert_eq!(error.code, -32022);

        // not a mesh (`:4260-4261`): a group with the mesh's id
        let fake = base().with_node(
            30,
            FakeNode {
                class: "group",
                id: Some(12),
                ..Default::default()
            },
        );
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("not a mesh");
        assert_eq!(error.data, r#"{"reason":"not a mesh","nodeID":12}"#);

        // the mesh is inside an instance (`:4263-4266`)
        let fake = base()
            .with_node(30, mesh_node(Some(50)))
            .with_node(50, FakeNode { class: "instance", ..Default::default() });
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("instance parent");
        assert_eq!(
            error.data,
            r#"{"reason":"mesh is inside a prefab/instance","nodeID":12}"#
        );

        // the mesh is locked (`:4268-4270`)
        let fake = base().with_node(30, FakeNode { locked: true, ..mesh_node(Some(10)) });
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("locked mesh");
        assert_eq!(error.data, r#"{"reason":"node is locked"}"#);
        assert_eq!(error.code, -32018);
    }

    /// `:4280-4285`: a mesh tied to another entity is refused naming the old parent
    /// (`parentNodeID`).
    #[test]
    fn tie_refuses_a_mesh_tied_to_another_entity_naming_the_old_parent() {
        let fake = two_nodes(5);
        fake.set_script(|s| s.class_exists = true);
        fake.set_node(
            30,
            FakeNode {
                parent: Some(20),
                ..fake.node(30)
            },
        );
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("mesh 30's parent is entity 20, not the target");
        assert_eq!(
            error.data,
            r#"{"reason":"already tied","nodeID":12,"parentNodeID":7}"#
        );
        assert_eq!(error.code, -32602);
    }

    /// `:4287-4296` (review-b M2): a retie refuses a locked OLD parent; the error names it.
    #[test]
    fn tie_retie_refuses_a_locked_old_parent() {
        let fake = two_nodes(5);
        fake.set_script(|s| s.class_exists = true);
        // mesh 30 is tied to entity 20 (id 7), which is locked
        fake.set_node(30, FakeNode { parent: Some(20), ..fake.node(30) });
        fake.set_node(
            20,
            FakeNode {
                locked: true,
                children: Some(vec![30]),
                ..fake.node(20)
            },
        );
        let error = run_tie(
            &fake,
            r#"{"class":"trigger_once","nodeIDs":[12],"allowRetie":true}"#,
        )
        .expect_err("the old parent is locked");
        assert_eq!(error.data, r#"{"reason":"node is locked","nodeID":7}"#);
        assert_eq!(error.code, -32018);
    }

    /// `:4300-4306` (review-b L5): an unreadable first-mesh transform is refused before the
    /// group opens.
    #[test]
    fn tie_refuses_an_unreadable_first_mesh_transform() {
        let fake = Fake::default().with_node(
            10,
            FakeNode {
                class: "world",
                id: None,
                origin: None,
                angles: None,
                parent: None,
                children: Some(vec![30]),
                gd: 0,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake.set_script(|s| s.class_exists = true);
        fake.set_node(
            30,
            FakeNode {
                class: "mesh",
                id: Some(12),
                origin: None,
                angles: Some([0.0; 3]),
                parent: Some(10),
                children: Some(Vec::new()),
                gd: 0,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("origin unreadable");
        assert_eq!(
            error.data,
            r#"{"reason":"the first mesh's origin/angles are unreadable"}"#
        );
        assert_eq!(error.code, -32603);
    }

    /// `:4312-4319` (FIX-B live L1): success WITHOUT a group when every mesh is already under
    /// the target; no undo_open, no modified, no AddChild, no recording.
    #[test]
    fn tie_noops_when_every_mesh_is_already_under_the_target() {
        let fake = two_nodes(5);
        // the mesh is under the entity (the target) already
        fake.set_node(
            30,
            FakeNode {
                parent: Some(20),
                ..fake.node(30)
            },
        );
        fake.set_node(
            20,
            FakeNode {
                children: Some(vec![30]),
                ..fake.node(20)
            },
        );
        let reply = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect("noop success");
        // The C's `tie_reply_body` sequence (`:4436-4479`), noop arm; the handle token is the
        // global counter's next value.
        assert_eq!(
            reply,
            format!(
                "{{\"nodeID\":7,\"handle\":\"{}\",\"class\":\"entity\",\"classname\":\"trigger_once\",\"created\":false,\"origin\":[3,3,3],\"meshes\":[{{\"nodeID\":12,\"origin\":[64,64,0]}}],\"alreadyTied\":[12],\"removedEmptyEntities\":[],\"noop\":true,\"undoLabel\":null}}",
                reply_token(&reply, 0)
            ),
        );
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("undo_open")),
            "no group opened: {:?}",
            fake.calls()
        );
        assert_eq!(*fake.modified.borrow(), 0, "no modified");
        assert!(fake.recorded.borrow().is_empty(), "no recording");
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("add_child")),
            "no AddChild"
        );
    }

    /// `:4211-4219` + `:4376-4437`: the full entityID-form path with the fake — one group, the
    /// recording BEFORE the AddChild, the modified flag, the reply bytes.
    #[test]
    fn tie_full_path_builds_the_c_reply_bytes() {
        let fake = two_nodes(5);
        fake.set_script(|s| {
            s.class_exists = true;
            s.add_ok = true;
        });
        let reply = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect("success");
        assert_eq!(
            reply,
            format!(
                "{{\"nodeID\":7,\"handle\":\"{}\",\"class\":\"entity\",\"classname\":\"trigger_once\",\"created\":false,\"origin\":[3,3,3],\"meshes\":[{{\"nodeID\":12,\"origin\":[64,64,0]}}],\"alreadyTied\":[],\"removedEmptyEntities\":[],\"noop\":false,\"undoLabel\":\"MCP: tie meshes to entity\"}}",
                reply_token(&reply, 0)
            ),
        );
        assert_eq!(
            fake.calls(),
            vec![
                // the entityID target's gates (`:4219-4230`)
                "gd 20",
                "locked 20",
                "parent 20",
                // the mesh's gates (`:4250-4270` + the already-tied read `:4272`)
                "parent 30",
                "locked 30",
                "parent 30",
                // the group and the reparent
                "undo_open doc=4 label=MCP: tie meshes to entity",
                "record_nodes hist=7 nodes=[20, 30]",
                "parent 30",
                "add_child ent=20 node=30",
                "parent 30",
                "modified doc=4",
                "classname 20",
            ],
            "the C's call order (`node_parent` is read after the AddChild, before `modified`)"
        );
    }

    /// `:4340-4362`: the class form creates the entity; a point class is caught by the
    /// defensive check and deleted in the same group, answering `undoGroupOpened`.
    #[test]
    fn tie_class_form_catches_a_point_class_after_the_create() {
        let fake = two_nodes(5);
        fake.set_node(
            100,
            FakeNode {
                class: "entity",
                id: Some(100),
                origin: Some([64.0, 64.0, 0.0]),
                angles: Some([0.0; 3]),
                parent: None,
                children: Some(Vec::new()),
                gd: 4, // a point class
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake.set_node(10, FakeNode { children: Some(vec![20, 30]), ..fake.node(10) });
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = vec![100];
        });
        let error = run_tie(&fake, r#"{"class":"info_target","nodeIDs":[12]}"#)
            .expect_err("a point class is caught");
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.data,
            r#"{"reason":"not a solid (brush) class","undoGroupOpened":true,"undoLabel":"MCP: tie meshes to entity"}"#
        );
        assert_eq!(*fake.deleted.borrow(), vec![100], "the created node is deleted");
    }

    /// `:4346-4350`: a failed CreateNodeByName answers `undoGroupOpened:true` (the group IS
    /// open), no maybeExecuted.
    #[test]
    fn tie_create_failure_answers_undo_group_opened() {
        let fake = two_nodes(5);
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = Vec::new();
        });
        let error = run_tie(&fake, r#"{"class":"trigger_once","nodeIDs":[12]}"#)
            .expect_err("CreateNodeByName failed");
        assert_eq!(error.code, -32603);
        assert_eq!(
            error.data,
            r#"{"reason":"CreateNodeByName failed (see bridge log)","undoGroupOpened":true,"undoLabel":"MCP: tie meshes to entity"}"#
        );
    }

    /// `:4389-4421` (review-b M6): an AddChild refusal after earlier ties ran reports the
    /// progress with maybeExecuted:true, id lists capped at TIE_ERR_LIST_CAP (10): 13 meshes,
    /// 12 land (12 > 10), the 13th is refused — the `tied` list names the first 10 landed ids.
    #[test]
    fn tie_add_child_refusal_reports_progress_capped() {
        let fake = two_nodes(5);
        let mut kids = Vec::new();
        kids.push(20); // the target entity itself must stay reachable from the world
        let ids: Vec<i32> = (0..13).map(|i| 20 + i).collect();
        for (i, id) in ids.iter().enumerate() {
            let handle = 40 + i;
            kids.push(handle);
            fake.set_node(
                handle,
                FakeNode {
                    class: "mesh",
                    id: Some(*id),
                    origin: Some([64.0, 64.0, 0.0]),
                    angles: Some([0.0; 3]),
                    parent: Some(10),
                    children: Some(Vec::new()),
                    gd: 0,
                    properties: None,
                    classname: Vec::new(),
                    locked: false,
                },
            );
        }
        fake.set_node(10, FakeNode { children: Some(kids), ..fake.node(10) });
        fake.set_node(
            20,
            FakeNode {
                children: Some(vec![30]),
                ..fake.node(20)
            },
        );
        fake.set_script(|s| {
            s.land = 12; // 12 ties land, the 13th refuses
        });
        let params = format!(
            r#"{{"entityID":7,"nodeIDs":[{}]}}"#,
            ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",")
        );
        let error = run_tie(&fake, &params).expect_err("the 13th tie refuses");
        assert_eq!(error.code, -32603);
        assert_eq!(
            error.data,
            format!(
                concat!(
                    r#"{{"reason":"AddChild refused a mesh; the ties before it in this group "#,
                    r#"already ran - verify with entity.get, do not retry","nodeID":{},"#,
                    r#""maybeExecuted":true,"tied":[{}],"removedEmptyEntities":[]}}"#
                ),
                32,
                (20..30).map(|id| id.to_string()).collect::<Vec<_>>().join(",")
            ),
            "the capped progress lists"
        );
        assert_eq!(*fake.modified.borrow(), 1, "DocSetModified before the refusal");
    }

    /// `:4426-4431`:
    #[test]
    fn tie_add_child_refusal_single() {
        let fake = two_nodes(5);
        fake.set_script(|s| s.add_ok = false);
        let error = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect_err("refused");
        assert_eq!(error.code, -32603);
        assert_eq!(
            error.data,
            r#"{"reason":"AddChild refused a mesh; the ties before it in this group already ran - verify with entity.get, do not retry","nodeID":12,"maybeExecuted":true,"tied":[],"removedEmptyEntities":[]}"#
        );
        assert_eq!(*fake.modified.borrow(), 1, "DocSetModified before the refusal");
    }

    /// `:4426-4431`: an old brush entity the tie empties (gd 5, no children left) is deleted
    /// in the same group and reported in `removedEmptyEntities`.
    #[test]
    fn tie_deletes_the_emptied_old_brush_entity() {
        // target entity 20 (id 7); old brush entity 50 (id 70, gd 5) holds mesh 30;
        // a second mesh 31 (id 13) sits under the world.
        let fake = two_nodes(5);
        fake.set_node(
            50,
            FakeNode {
                class: "entity",
                id: Some(70),
                origin: Some([8.0; 3]),
                angles: Some([0.0; 3]),
                parent: Some(10),
                children: Some(vec![30]),
                gd: 5,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake.set_node(
            30,
            FakeNode {
                parent: Some(50),
                ..fake.node(30)
            },
        );
        let handle51 = 51;
        fake.set_node(
            handle51,
            FakeNode {
                class: "mesh",
                id: Some(13),
                origin: Some([80.0, 80.0, 0.0]),
                angles: Some([0.0; 3]),
                parent: Some(10),
                children: Some(Vec::new()),
                gd: 0,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake.set_node(10, FakeNode { children: Some(vec![20, 30, 50, 51]), ..fake.node(10) });
        fake.set_script(|s| s.add_ok = true);
        let reply = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12,13],"allowRetie":true}"#)
            .expect("success");
        assert!(
            reply.contains(r#""removedEmptyEntities":[70]"#),
            "the emptied old entity's id is reported: {reply}"
        );
        assert_eq!(*fake.deleted.borrow(), vec![50], "DeleteNode ran on the old entity");
    }

    /// `tie_ancestors_ok`'s 64-bound: a 65-deep group chain is refused, not guessed.
    #[test]
    fn tie_ancestor_walk_bounded_at_64() {
        let fake = two_nodes(5);
        // chain entity 20 -> groups 60..(60+65) so the walk runs out of budget
        fake.set_node(
            20,
            FakeNode {
                parent: Some(100),
                ..fake.node(20)
            },
        );
        for d in 0..70 {
            fake.set_node(
                100 + d,
                FakeNode {
                    class: "group",
                    parent: Some(100 + d + 1),
                    children: Some(Vec::new()),
                    ..Default::default()
                },
            );
        }
        let error = run_tie(&fake, r#"{"entityID":7,"nodeIDs":[12]}"#).expect_err("too deep");
        assert_eq!(
            error.data,
            r#"{"reason":"entity is inside a prefab/instance","nodeID":7}"#
        );
    }

    // -----------------------------------------------------------------------------------------
    // entity.untie
    // -----------------------------------------------------------------------------------------

    /// `op_entity_untie`'s param gates (`bridge/mcp_bridge.c:4489-4501`).
    #[test]
    fn untie_refuses_its_params_in_the_c_order() {
        let cases: &[(&str, &str, &str)] = &[
            ("nodeID required", "nodeID required", "-32602"),
            (
                r#"{"nodeID":"x"}"#,
                "nodeID: a non-negative int up to 2147483647 is required",
                "-32602",
            ),
            (
                r#"{"nodeID":-1}"#,
                "nodeID: a non-negative int up to 2147483647 is required",
                "-32602",
            ),
            (
                r#"{"nodeID":2147483648}"#,
                "nodeID: a non-negative int up to 2147483647 is required",
                "-32602",
            ),
        ];
        for (params, reason, code) in cases {
            let fake = Fake::default();
            let error = run_untie(&fake, Some(params)).expect_err(reason);
            assert_eq!(error.code.to_string(), *code, "{params}");
            assert_eq!(error.data, format!("{{\"reason\":\"{reason}\"}}"), "{params}");
            assert!(fake.calls().is_empty(), "no game access: {:?}", fake.calls());
        }
        // No params at all: the same `nodeID required`.
        let fake = Fake::default();
        let error = run_untie(&fake, None).expect_err("nodeID required");
        assert_eq!(error.data, r#"{"reason":"nodeID required"}"#);
        assert!(fake.calls().is_empty());
    }

    /// `:4499-4524`: the entity gates after the params.
    #[test]
    fn untie_entity_gates() {
        // not found: geo_entity_node's -32022 (`:4498`)
        let fake = Fake::default();
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("not found");
        assert_eq!(error.code, -32022);
        assert_eq!(
            error.data,
            r#"{"reason":"no node with this nodeID in the live document"}"#
        );

        // not an entity: geo_entity_node's -32602
        let fake = two_nodes(5);
        let error = run_untie(&fake, Some(r#"{"nodeID":12}"#)).expect_err("a mesh id");
        assert_eq!(error.code, -32602);
        assert_eq!(error.data, r#"{"reason":"not an entity"}"#);

        // a point entity (gd 4): "not a brush entity" (`:4499-4501`)
        let fake = two_nodes(4);
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("point class");
        assert_eq!(error.data, r#"{"reason":"not a brush entity"}"#);

        // locked (`:4503-4505`)
        let fake = two_nodes(5);
        fake.set_node(20, FakeNode { locked: true, ..fake.node(20) });
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("locked");
        assert_eq!(error.data, r#"{"reason":"node is locked"}"#);
        assert_eq!(error.code, -32018);

        // no children (`:4511-4513`)
        let fake = two_nodes(5);
        fake.set_node(20, FakeNode { children: Some(Vec::new()), ..fake.node(20) });
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("no children");
        assert_eq!(error.data, r#"{"reason":"not a brush entity"}"#);

        // a corrupt child count (`:4516-4520`)
        let fake = two_nodes(5);
        fake.set_node(20, FakeNode { children: None, ..fake.node(20) });
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("unreadable");
        assert_eq!(error.code, -32603);
        assert_eq!(error.data, r#"{"reason":"child list unreadable"}"#);

        // no parent (`:4539-4542`, review-b L8)
        let fake = two_nodes(5);
        fake.set_node(20, FakeNode { parent: None, children: Some(vec![30]), ..fake.node(20) });
        let error = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect_err("no parent");
        assert_eq!(error.code, -32603);
        assert_eq!(error.data, r#"{"reason":"entity has no parent"}"#);
    }

    /// `:4545-4559`: the move runs with the children for the reply, the modified flag and the
    /// exact bytes.
    #[test]
    fn untie_moves_to_world_and_builds_the_c_reply() {
        let fake = two_nodes(5);
        fake.set_node(20, FakeNode { children: Some(vec![30]), ..fake.node(20) });
        fake.set_node(30, FakeNode { parent: Some(20), ..fake.node(30) });
        let reply = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect("success");
        assert_eq!(
            reply,
            r#"{"nodeID":7,"meshes":[12],"entityDeleted":true,"undoLabel":"Untie from entity"}"#
        );
        assert_eq!(*fake.moved.borrow(), vec![20], "DocMoveToWorld with the entity");
        assert_eq!(*fake.modified.borrow(), 1, "DocSetModified");
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("undo_open")),
            "no undo_open: MoveToWorld opens its own group"
        );
        // the entity was never touched again after the call
        assert!(fake.calls().iter().position(|call| call.starts_with("move_to_world")).unwrap()
            > fake.calls().iter().position(|call| call.starts_with("cc")).unwrap());
    }

    /// `:4516-4520` (review-b L7): more children than the reply lists is meshesTruncated, not
    /// an internal fault.
    #[test]
    fn untie_truncates_the_mesh_list() {
        let fake = two_nodes(5);
        let mut kids = Vec::new();
        let mut node_ids = Vec::new();
        for i in 0..1030 {
            let handle = 1000 + i;
            kids.push(handle);
            node_ids.push(5000 + i);
            let id = (5000 + i) as i32;
            fake.set_node(
                handle,
                FakeNode {
                    class: "mesh",
                    id: Some(id),
                    origin: Some([1.0; 3]),
                    angles: Some([0.0; 3]),
                    parent: Some(20),
                    children: Some(Vec::new()),
                    gd: 0,
                    properties: None,
                    classname: Vec::new(),
                    locked: false,
                },
            );
        }
        fake.set_node(20, FakeNode { children: Some(kids), ..fake.node(20) });
        let reply = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect("success");
        assert!(
            reply.starts_with(r#"{"nodeID":7,"meshes":[5000,"#),
            "the first ids are listed: {reply}"
        );
        assert!(reply.contains("6023"), "the last listed id (5000 + 1023) is there");
        assert!(!reply.contains("6024"), "past the cap nothing is listed");
        assert!(
            reply.contains(r#""meshesTruncated":true"#),
            "truncation is reported: {reply}"
        );
    }

    /// `:4532-4534`: a child whose nodeID is negative is skipped, exactly like an unreadable one
    /// (the C's `if (cid < 0) continue;`) — never listed as `-1`.
    #[test]
    fn untie_skips_children_with_a_negative_node_id() {
        let fake = two_nodes(5);
        // one real mesh (id 12), then a negative id, an unreadable id and another negative id
        fake.set_node(
            41,
            FakeNode {
                class: "mesh",
                id: Some(-1),
                ..FakeNode::default()
            },
        );
        fake.set_node(
            42,
            FakeNode {
                class: "mesh",
                id: None,
                ..FakeNode::default()
            },
        );
        fake.set_node(
            43,
            FakeNode {
                class: "mesh",
                id: Some(-7),
                ..FakeNode::default()
            },
        );
        fake.set_node(
            20,
            FakeNode { children: Some(vec![30, 41, 42, 43]), ..fake.node(20) },
        );
        let reply = run_untie(&fake, Some(r#"{"nodeID":7}"#)).expect("success");
        assert_eq!(
            reply,
            r#"{"nodeID":7,"meshes":[12],"entityDeleted":true,"undoLabel":"Untie from entity"}"#
        );
    }

    // -----------------------------------------------------------------------------------------
    // edit.batch
    // -----------------------------------------------------------------------------------------

    /// The fake tree for the batch bodies: world(10) with a pre-built brush entity (20) and
    /// one mesh (40); `creates` answers entity 20 for every create.
    fn batch_tree() -> Fake {
        let fake = Fake::default();
        fake.set_node(
            10,
            FakeNode {
                class: "world",
                id: None,
                origin: None,
                angles: None,
                parent: None,
                children: Some(vec![20, 40]),
                gd: 0,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake.set_node(
            20,
            FakeNode {
                class: "entity",
                id: Some(7),
                origin: Some([3.0; 3]),
                angles: Some([0.0; 3]),
                parent: Some(10),
                children: Some(Vec::new()),
                gd: 5,
                properties: Some(400),
                classname: b"trigger_once".to_vec(),
                locked: false,
            },
        );
        fake.set_node(
            40,
            FakeNode {
                class: "mesh",
                id: Some(12),
                origin: Some([64.0, 64.0, 0.0]),
                angles: Some([0.0; 3]),
                parent: Some(10),
                children: Some(Vec::new()),
                gd: 0,
                properties: None,
                classname: Vec::new(),
                locked: false,
            },
        );
        fake
    }

    /// `batch_undo_label` (`mcp_bridge.c:2468-2471`): the label follows the create scan.
    #[test]
    fn batch_undo_label_follows_the_create_scan() {
        assert_eq!(
            batch_undo_label(br#"[{"method":"entity.create"},{"method":"entity.setKeyValues"}]"#)
                .to_bytes(),
            b"MCP: batch"
        );
        assert_eq!(
            batch_undo_label(br#"[{"method":"entity.setKeyValues"}]"#).to_bytes(),
            b"MCP: set keyvalues"
        );
        assert_eq!(batch_undo_label(b"[]").to_bytes(), b"MCP: set keyvalues");
    }

    /// `batch_subst_handle` (`mcp_bridge.c:2474-2504`): the `"$N"` substitution and its
    /// refusals.
    #[test]
    fn batch_subst_handle_resolves_the_op_refs() {
        let row = |token: &[u8]| {
            let mut row = [0u8; 16];
            let end = token.len();
            row[..end].copy_from_slice(token);
            row
        };
        let handles = [row(b"n41"), row(b"n42")];

        // substitution rewrites the value in place
        let mut params = [0u8; 1536];
        params[..26].copy_from_slice(b"{\"handle\":\"$0\",\"nodeID\":7}");
        batch_subst_handle(&mut params, &handles, 2);
        assert_eq!(&params[..27], b"{\"handle\":\"n41\",\"nodeID\":7}");

        // a non-$ handle is untouched
        let mut params = [0u8; 1536];
        params[..15].copy_from_slice(b"{\"handle\":\"n3\"}");
        batch_subst_handle(&mut params, &handles, 2);
        assert_eq!(&params[..15], b"{\"handle\":\"n3\"}");

        // out of range: not substituted (the C would read outside the table)
        let mut params = [0u8; 1536];
        params[..21].copy_from_slice(b"{\"handle\":\"$9\",\"a\":1}");
        batch_subst_handle(&mut params, &handles, 2);
        assert_eq!(&params[..21], b"{\"handle\":\"$9\",\"a\":1}");

        // an unset row (empty token): not substituted
        let handles = [row(b"n41"), [0u8; 16]];
        let mut params = [0u8; 1536];
        params[..15].copy_from_slice(b"{\"handle\":\"$1\"}");
        batch_subst_handle(&mut params, &handles, 2);
        assert_eq!(&params[..15], b"{\"handle\":\"$1\"}");

        // a handle value that is not a string: untouched
        let mut params = [0u8; 1536];
        params[..12].copy_from_slice(b"{\"handle\":5}");
        batch_subst_handle(&mut params, &handles, 2);
        assert_eq!(&params[..12], b"{\"handle\":5}");

        // trailing members survive the rewrite
        let handles = [row(b"n41")];
        let mut params = [0u8; 1536];
        let text = b"{\"nodeID\":1,\"handle\":\"$0\",\"values\":{\"a\":2}}";
        params[..text.len()].copy_from_slice(text);
        batch_subst_handle(&mut params, &handles, 1);
        assert_eq!(
            &params[..params.iter().position(|b| *b == 0).unwrap()],
            &b"{\"nodeID\":1,\"handle\":\"n41\",\"values\":{\"a\":2}}"[..]
        );
    }

    /// `:5878` + `:5901`/`:5932`: an `edit.batch` opens exactly ONE undo group for the whole run.
    ///
    /// The C's per-op bodies are `op_entity_create` (`:2167-2171`) and `op_set_keyvalues`
    /// (`:2447-2452`), neither of which opens a group — only the standalone arms do
    /// (`:5726`, `:5839`). Before this fix the Rust batch called the arm-shaped bodies, so a
    /// 3-create batch left undo depth 3 named "MCP: create entity" (CONFIRMED live) instead of
    /// the single "MCP: batch" the C leaves.
    #[test]
    fn edit_batch_creates_open_exactly_one_group() {
        let fake = batch_tree();
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = vec![20, 20, 20];
        });
        let reply = run_batch_arm(
            &fake,
            &batch_params(concat!(
                r#"[{"method":"entity.create","params":{"class":"trigger_once"}},"#,
                r#"{"method":"entity.create","params":{"class":"trigger_once"}},"#,
                r#"{"method":"entity.create","params":{"class":"trigger_once"}}]"#
            )),
            "b7",
        );
        assert!(reply.contains(r#""failedIndex":null"#), "{reply}");
        assert_eq!(
            fake.undo_labels(),
            vec![b"MCP: batch".to_vec()],
            "one group for the whole batch, never one per create: {:?}",
            fake.calls()
        );

        // a create followed by a keyValues op on that node: still the one batch group (the C's
        // batch label stays "MCP: batch" because the scan sees a create)
        let fake = batch_tree();
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = vec![20];
        });
        let reply = run_batch_arm(
            &fake,
            &batch_params(concat!(
                r#"[{"method":"entity.create","params":{"class":"trigger_once"}},"#,
                r#"{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"a":1}}}]"#
            )),
            "b8",
        );
        assert!(reply.contains(r#""failedIndex":null"#), "{reply}");
        assert_eq!(fake.undo_labels(), vec![b"MCP: batch".to_vec()], "{:?}", fake.calls());
    }

    /// `:5878`: a batch of nothing but setKeyValues ops takes the keyvalue label.
    #[test]
    fn edit_batch_keyvalue_only_batch_takes_the_keyvalue_label() {
        let fake = batch_tree();
        let reply = run_batch_arm(
            &fake,
            &batch_params(
                r#"[{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"targetname":"x"}}}]"#,
            ),
            "b2",
        );
        assert!(reply.contains(r#""applied":{"targetname":true}"#), "{reply}");
        assert_eq!(fake.undo_labels(), vec![b"MCP: set keyvalues".to_vec()]);
        // the exact reply: the C's `set_keyvalues_apply` object, the batch's `results` /
        // `failedIndex` wrapper and `sb_add_doc_object` (`:2432-2444`, `:5988-6005`), with the
        // host fingerprint `d0`/0 and the absent game's zero undo info.
        let expected = format!(
            concat!(
                r#"{{"jsonrpc":"2.0","id":"b2","bridge":{{"protocol":1,"build":"{}"}},"#,
                r#""result":{{"results":[{{"applied":{{"targetname":true}},"warnings":[],"#,
                r#""previous":{{"targetname":null}},"previousTruncated":[],"#,
                r#""valueUndo":"recorded"}}],"failedIndex":null,"#,
                r#""doc":{{"id":"d0","topSerial":0,"undoDepth":0}}}}}}"#,
                "\n"
            ),
            crate::BRIDGE_BUILD
        );
        assert_eq!(reply, expected);
    }

    /// `:5951`: an unknown method inside the batch is the row's inner NotImplemented.
    #[test]
    fn edit_batch_unknown_method_is_an_inner_not_implemented_row() {
        let fake = batch_tree();
        let reply = run_batch_arm(
            &fake,
            &batch_params(r#"[{"method":"pet.drop","params":{}}]"#),
            "b3",
        );
        assert!(
            reply.contains(concat!(
                r#""error":{"code":-32012,"message":"PartialFailure","#,
                r#""data":{"results":[{"error":{"code":-32011,"message":"NotImplemented","data":{"reason":"pet.drop"}}}],"failedIndex":0}"#
            )),
            "the inner NotImplemented row: {reply}"
        );
    }

    /// `:5972-5974` + `batch_record_decision`: the node the batch created is recorded by its
    /// create; a setKeyValues op on it (by nodeID) skips the record; an op on another node
    /// records once and a second op on it skips.
    #[test]
    fn edit_batch_record_decision_skips_recorded_nodes() {
        let fake = batch_tree();
        // the batch's create answers the existing entity (handle 20, node id 7)
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = vec![20];
        });
        let reply = run_batch_arm(
            &fake,
            &batch_params(concat!(
                r#"[{"method":"entity.create","params":{"class":"trigger_once"}},"#,
                r#"{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"a":1}}},"#,
                r#"{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"b":2}}}]"#
            )),
            "b4",
        );
        assert!(reply.contains(r#""failedIndex":null"#), "{reply}");
        // the create recorded the node in this group; both setKeyValues ops take rec=false.
        let calls = fake.calls();
        assert_eq!(
            calls.iter().filter(|call| call.starts_with("record_tree")).count(),
            0,
            "no setKeyValues op recorded: node 7 was covered by its create: {:?}",
            calls
        );
        // now two ops on an existing node that the batch did NOT create: the first records,
        // the second skips
        let fake = batch_tree();
        let reply = run_batch_arm(
            &fake,
            &batch_params(concat!(
                r#"[{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"a":1}}},"#,
                r#"{"method":"entity.setKeyValues","params":{"nodeID":7,"values":{"b":2}}}]"#
            )),
            "b5",
        );
        assert!(reply.contains(r#""failedIndex":null"#), "{reply}");
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|call| call.starts_with("record_tree"))
                .count(),
            1,
            "the first op recorded once, the second took the 0 branch: {:?}",
            fake.calls()
        );
    }

    /// The write gate's three checks come before the `ops` member (`:5855-5885`), and the
    /// `ops` gate itself is a bare `-32602` with no `data` (`:5883-5885`).
    #[test]
    fn edit_batch_gates_before_it_looks_at_ops() {
        let fake = batch_tree();
        // no params at all: the gate's `missing` (`:1930`)
        let reply = edit_batch_with(&fake, &batch_request(None, "g0"));
        assert!(
            reply.contains(r#""code":-32004,"message":"DocumentChanged","data":{"reason":"missing"}"#),
            "no params: {reply}"
        );
        // params without expectDoc: the same `missing`
        let reply = run_batch_arm(&fake, r#"{"ops":[]}"#, "g1");
        assert!(
            reply.contains(r#""code":-32004,"message":"DocumentChanged","data":{"reason":"missing"}"#),
            "no expectDoc: {reply}"
        );
        // a stale epoch (`:1942`)
        let reply = run_batch_arm(
            &fake,
            r#"{"expectDoc":{"id":"d0","epoch":7},"ops":[]}"#,
            "g2",
        );
        assert!(
            reply.contains(r#""code":-32004,"message":"DocumentChanged","data":{"reason":"epoch"}"#),
            "stale epoch: {reply}"
        );
        // a wrong document id (`:1941`)
        let reply = run_batch_arm(
            &fake,
            r#"{"expectDoc":{"id":"d9","epoch":0},"ops":[]}"#,
            "g3",
        );
        assert!(
            reply.contains(r#""code":-32004,"message":"DocumentChanged","data":{"reason":"doc_id"}"#),
            "wrong doc id: {reply}"
        );
        // the gate passed, the `ops` member is missing: a bare `invalid params: ops`
        let reply = run_batch_arm(&fake, r#"{"expectDoc":{"id":"d0","epoch":0}}"#, "g4");
        assert!(
            reply.contains(r#""code":-32602,"message":"invalid params: ops"}"#),
            "no ops: {reply}"
        );
        assert!(!reply.contains("\"data\":"), "a bare error carries no data: {reply}");
        // `ops` is not an array
        let reply = run_batch_arm(
            &fake,
            r#"{"expectDoc":{"id":"d0","epoch":0},"ops":{"method":"entity.create"}}"#,
            "g5",
        );
        assert!(
            reply.contains(r#""code":-32602,"message":"invalid params: ops"}"#),
            "an object ops: {reply}"
        );
        // none of it reached the game or opened a group
        assert!(
            !fake.calls().iter().any(|call| call.starts_with("undo_open")),
            "no group: {:?}",
            fake.calls()
        );
    }

    /// `:5986-5990`: a batch whose results overflow the 256 KB builder answers PartialFailure
    /// with `failedIndex:null` and the verify-with-entity.find reason.
    #[test]
    fn edit_batch_overflow_answers_partial_failure_null_failed_index() {
        let fake = batch_tree();
        // every create succeeds with the same node: the replies alone pass 256 KB
        fake.set_script(|s| {
            s.class_exists = true;
            s.creates = vec![20; 3200];
        });
        let mut ops = String::from("[");
        for _ in 0..3000 {
            ops.push_str(r#"{"method":"entity.create","params":{"class":"trigger_once"}},"#);
        }
        ops.push_str(r#"{"method":"entity.create","params":{"class":"trigger_once"}}]"#);
        let reply = run_batch_arm(&fake, &batch_params(&ops), "b6");
        assert!(
            reply.contains(concat!(
                r#""code":-32012,"message":"PartialFailure","#,
                r#""data":{"failedIndex":null,"reason":"batch result too large; the ops ran - verify with entity.find, do not retry"}"#
            )),
            "the overflow object: {reply}"
        );
    }
}
