//! The geometry writes: `mesh.create`, `mesh.setGeometry`, `mesh.setMaterial` (M4 lane MESH),
//! plus `mesh.setFaceTexture` (bridge m3.14).
//!
//! This is the C bridge's polygon-soup region: the parser (`geom_parse`,
//! `mcp_bridge.c:3188-3271`), the mesh reset/build helpers (`geo_reset_mesh` `:4671`,
//! `geo_build` `:4705`), the reply summary (`geo_mesh_summary` `:4736`), the three op bodies
//! (`:5392`, `:5428`, `:5449`) and the `geo_dispatch` arm tail (`:5972-5977`).
//!
//! # How it is split
//!
//! Everything that touches the document or the polygon mesh is one [`Mesh`] method per C
//! primitive; the production implementation is [`GameMesh`] (Windows only: the generated
//! `mapdoclib` classes are `cfg(windows)`). The parser, the error mapping and every result
//! builder are plain functions over strings, numbers and the small structs the reader fills
//! ([`GGeom`], [`MeshSummary`]), so the host tests hold them to the C's `sb_add` sequences and
//! its `why` strings byte for byte.
//!
//! # Statics
//!
//! Nothing new: the module composes `ops::hammer`'s bind table, the document fingerprint and
//! the undo history. Every body runs on the tools main thread (the tick's drain), like the C.

use crate::entity_json::{json_find, json_int_top, json_string, ConnSpec, Sb};
use crate::log;
use crate::ops::hammer::{
    active_doc_with, addf3, byte_at, cstr, expect_doc_ok, fingerprint_ids, game_write, geo_find,
    jnum, jws, seq, write_finish, write_gate, AttrInfo,
    Chain, Conn, CreateTarget, Doc, FilterFields, HistoryInfo, Node, Nodes, PrevValue, Write,
    NODEID_MAX, OP_RESULT_CAP,
};
use crate::ops::OpErr;
use crate::queue::Request;
use core::ffi::CStr;

/// `GEO_MAX_VERTS` (`mcp_bridge.c:3119`).
const GEO_MAX_VERTS: usize = 65_536;
/// `GEO_MAX_FACES` (`mcp_bridge.c:3120`).
const GEO_MAX_FACES: usize = 65_536;
/// `GEO_MAX_IDX` (`mcp_bridge.c:3121`): `1u << 20`.
const GEO_MAX_IDX: usize = 1 << 20;
/// `GEO_MAX_MATS` (`mcp_bridge.c:3122`).
const GEO_MAX_MATS: usize = 64;
/// `GEO_UV_MODE` (`mcp_bridge.c:3124`): what Hammer's Polygon tool passes.
#[cfg(windows)]
const GEO_UV_MODE: i32 = 2;
/// One material string's capacity: `char mats[GEO_MAX_MATS][260]` / `char defmat[260]`
/// (`mcp_bridge.c:3133-3135`).
const GEO_MAT_CAP: usize = 260;
/// `GEO_MAX_FACES * 4` (`:4676`, `:5456`): the face count above which a mesh is refused.
const GEO_FACE_CAP: i32 = (GEO_MAX_FACES * 4) as i32;

/// The C's `S2MeshHandle` (`s2sdk.h:64`): 16 opaque bytes.
pub(crate) type MeshHandle = [u8; 16];

/// What Hammer's half-edge accessors answer for an invalid/stale handle: index `0x3FFFFF`,
/// generation 0 (re-report R1). Only the low `u32` of a handle matters to `mesh.get`.
#[allow(dead_code)] // the Windows live impl and the host tests are the callers
pub(crate) const INVALID_HANDLE: MeshHandle = {
    let mut h = [0u8; 16];
    h[0] = 0xFF;
    h[1] = 0xFF;
    h[2] = 0x3F;
    h
};

/// A handle's low `u32` (22-bit index, 10-bit generation): the key `mesh.get` matches
/// vertices and edges by.
fn handle_key(h: &MeshHandle) -> u32 {
    u32::from_le_bytes([h[0], h[1], h[2], h[3]])
}

/// Index `0x3FFFFF`: the invalid handle.
fn handle_invalid(h: &MeshHandle) -> bool {
    handle_key(h) & 0x3F_FFFF == 0x3F_FFFF
}

// ---------------------------------------------------------------------------------------------
// The parsed geometry (`GGeom`, `mcp_bridge.c:3126-3136`)
// ---------------------------------------------------------------------------------------------

/// The C's `GGeom` (`mcp_bridge.c:3126-3136`): the polygon soup and the materials a request
/// names. The C allocates five flat arrays up front with `geo_alloc` (`:3189-3194`); the Rust
/// port keeps the same capacities as `Vec`s (heap, main thread) and the same `nv`/`nf`/`nidx`
/// cursors, so every cap check and every index is the C's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GGeom {
    /// The vertex count (`g->nv`).
    pub nv: usize,
    /// The face count (`g->nf`).
    pub nf: usize,
    /// The index count (`g->nidx`).
    pub nidx: usize,
    /// `g->v`: the vertices.
    pub v: Vec<[f32; 3]>,
    /// `g->fstart`: each face's first index into [`GGeom::idx`].
    pub fstart: Vec<i32>,
    /// `g->fcount`: each face's index count.
    pub fcount: Vec<i32>,
    /// `g->fmat`: each face's material slot, -1 for the default.
    pub fmat: Vec<i32>,
    /// `g->idx`: every face's indices, concatenated.
    pub idx: Vec<i32>,
    /// `g->nmat`: the distinct non-default materials seen so far.
    pub nmat: usize,
    /// `g->mats`: those materials, in first-seen order.
    pub mats: Vec<Vec<u8>>,
    /// `g->defmat`: the `material` member.
    pub defmat: Vec<u8>,
}

impl GGeom {
    /// The C's `geo_alloc(sizeof(GGeom))` + five zeroed arrays (`:3189-3194`).
    fn new() -> Self {
        Self {
            nv: 0,
            nf: 0,
            nidx: 0,
            v: vec![[0.0f32; 3]; GEO_MAX_VERTS],
            fstart: vec![0i32; GEO_MAX_FACES],
            fcount: vec![0i32; GEO_MAX_FACES],
            fmat: vec![0i32; GEO_MAX_FACES],
            idx: vec![0i32; GEO_MAX_IDX],
            nmat: 0,
            mats: Vec::new(),
            defmat: Vec::new(),
        }
    }
}

/// `geo_mat_index` (`mcp_bridge.c:3180-3186`): the slot for `m`, adding it when new; `-2` when
/// all 64 slots are taken (the C's refusal).
fn geo_mat_index(g: &mut GGeom, m: &[u8]) -> i32 {
    for i in 0..g.nmat {
        if seq(&g.mats[i], m) {
            return i as i32;
        }
    }
    if g.nmat >= GEO_MAX_MATS {
        return -2;
    }
    // `cpy(g->mats[g->nmat], m, sizeof g->mats[0])` (`:3184`): `m` is a `jstr` copy already
    // bounded by 259 bytes, so the copy is the same bytes.
    g.mats.push(cstr(m).to_vec());
    g.nmat += 1;
    (g.nmat - 1) as i32
}

/// `jstr` (`mcp_bridge.c:3160-3174`): the JSON string at `start` (whitespace skipped first)
/// copied into `out` with only `\"`, `\\` and `\/` escapes, NUL-terminated.
///
/// Answers the position after the closing quote, or `None` for the C's `0`: not a string, an
/// unsupported escape, an unterminated string, or a string that does not fit `out` (`n + 1 >=
/// cap` fails, the C's own rule). This is the C's stricter reader, not `json_string_at`.
fn jstr(s: &[u8], start: usize, out: &mut [u8]) -> Option<usize> {
    let mut p = jws(s, start);
    if byte_at(s, p) != b'"' {
        return None;
    }
    p += 1;
    let mut n = 0usize;
    while byte_at(s, p) != 0 && byte_at(s, p) != b'"' {
        let mut c = byte_at(s, p);
        p += 1;
        if c == b'\\' {
            c = byte_at(s, p);
            p += 1;
            if c != b'"' && c != b'\\' && c != b'/' {
                return None;
            }
        }
        if n + 1 >= out.len() {
            return None;
        }
        out[n] = c;
        n += 1;
    }
    if byte_at(s, p) != b'"' {
        return None;
    }
    out[n] = 0;
    Some(p + 1)
}

/// `geom_parse` (`mcp_bridge.c:3188-3271`): the whole `{vertices, faces, material,
/// faceMaterials}` validation, in the C's order and with the C's `why` strings.
///
/// Every refusal is the C's `*why` text. The C's "out of memory" branches cannot happen here
/// (a `Vec` aborts instead of answering null), which is the one unreachable difference.
pub(crate) fn geom_parse(params: &[u8]) -> Result<GGeom, &'static str> {
    let mut g = GGeom::new();

    // :3196-3198
    let mut p = match json_find(params, b"vertices") {
        Some(p) if byte_at(params, p) == b'[' => p,
        _ => return Err("vertices: [[x,y,z],...] required"),
    };
    p = jws(params, p + 1);
    while byte_at(params, p) == b'[' {
        if g.nv >= GEO_MAX_VERTS {
            return Err("too many vertices (max 65536)"); // :3200
        }
        let mut c = [0.0f64; 3];
        p += 1;
        for (k, slot) in c.iter_mut().enumerate() {
            // :3204-3208
            let Some((next, value)) = jnum(params, p) else {
                return Err("vertices: each must be [x,y,z] with |coord| < 1e6");
            };
            if !(value > -1.0e6 && value < 1.0e6) {
                return Err("vertices: each must be [x,y,z] with |coord| < 1e6");
            }
            *slot = value;
            p = jws(params, next);
            if k < 2 {
                if byte_at(params, p) != b',' {
                    return Err("vertices: each must be [x,y,z]"); // :3210-3211
                }
                p += 1;
            }
        }
        if byte_at(params, p) != b']' {
            return Err("vertices: each must be [x,y,z]"); // :3213
        }
        g.v[g.nv] = [c[0] as f32, c[1] as f32, c[2] as f32]; // :3214
        g.nv += 1;
        p = jws(params, p + 1);
        if byte_at(params, p) == b',' {
            p = jws(params, p + 1);
        }
    }
    if byte_at(params, p) != b']' || g.nv < 3 {
        return Err("vertices: at least 3 [x,y,z]"); // :3219
    }

    // :3220-3222
    p = match json_find(params, b"faces") {
        Some(p) if byte_at(params, p) == b'[' => p,
        _ => return Err("faces: [[i,j,k,...],...] required"),
    };
    p = jws(params, p + 1);
    while byte_at(params, p) == b'[' {
        if g.nf >= GEO_MAX_FACES {
            return Err("too many faces (max 65536)"); // :3225
        }
        g.fstart[g.nf] = g.nidx as i32; // :3226
        p = jws(params, p + 1);
        while byte_at(params, p) != b']' {
            // :3229-3231
            let Some((next, d)) = jnum(params, p) else {
                return Err("faces: vertex indices must be integers in range");
            };
            if d < 0.0 || d >= g.nv as f64 || d != f64::from(d as i32) {
                return Err("faces: vertex indices must be integers in range");
            }
            if g.nidx >= GEO_MAX_IDX {
                return Err("too many face indices"); // :3232
            }
            g.idx[g.nidx] = d as i32;
            g.nidx += 1;
            p = jws(params, next);
            if byte_at(params, p) == b',' {
                p = jws(params, p + 1);
            } else if byte_at(params, p) != b']' {
                return Err("faces: malformed"); // :3236
            }
        }
        g.fcount[g.nf] = (g.nidx as i32) - g.fstart[g.nf]; // :3239
        if g.fcount[g.nf] < 3 {
            return Err("faces: each needs at least 3 vertices"); // :3240
        }
        g.fmat[g.nf] = -1; // :3241
        g.nf += 1;
        p = jws(params, p + 1);
        if byte_at(params, p) == b',' {
            p = jws(params, p + 1);
        }
    }
    if byte_at(params, p) != b']' || g.nf < 1 {
        return Err("faces: at least one face"); // :3245
    }

    // :3246-3249
    let mut defmat = [0u8; GEO_MAT_CAP];
    if !json_string(params, b"material", &mut defmat) || defmat[0] == 0 {
        return Err("material: a material path, e.g. materials/dev/reflectivity_30.vmat");
    }
    g.defmat = cstr(&defmat).to_vec();

    // :3250-3271
    if let Some(start) = json_find(params, b"faceMaterials") {
        if byte_at(params, start) == b'[' {
            p = jws(params, start + 1);
            let mut fi = 0usize;
            while byte_at(params, p) != b']' {
                if fi >= g.nf {
                    return Err("faceMaterials: longer than faces"); // :3255
                }
                if byte_at(params, p) == b'n' {
                    // :3256-3257: the C checks the four bytes of `null` itself.
                    if byte_at(params, p + 1) != b'u'
                        || byte_at(params, p + 2) != b'l'
                        || byte_at(params, p + 3) != b'l'
                    {
                        return Err("faceMaterials: strings or null");
                    }
                    p += 4;
                } else {
                    let mut m = [0u8; GEO_MAT_CAP];
                    let Some(next) = jstr(params, p, &mut m) else {
                        return Err("faceMaterials: strings or null"); // :3261
                    };
                    p = next;
                    let m = cstr(&m);
                    if !m.is_empty() && !seq(m, &g.defmat) {
                        let mi = geo_mat_index(&mut g, m);
                        if mi < 0 {
                            return Err("faceMaterials: at most 64 distinct materials"); // :3265
                        }
                        g.fmat[fi] = mi; // :3266
                    }
                }
                fi += 1;
                p = jws(params, p);
                if byte_at(params, p) == b',' {
                    p = jws(params, p + 1);
                } else if byte_at(params, p) != b']' {
                    return Err("faceMaterials: malformed"); // :3270
                }
            }
        }
    }
    Ok(g)
}

// ---------------------------------------------------------------------------------------------
// The live document and mesh, one method per C primitive
// ---------------------------------------------------------------------------------------------

/// The C primitives the three bodies reach: the document root, the mesh element, the polygon
/// mesh's component counts and the five bound Hammer functions (`CreateMeshElement`,
/// `MeshInitFromPolygons`, `AssignMaterialToFaces`, `CollectHandles`, `MeshDeleteFaces`,
/// `:1674-1678`).
///
/// `Write` supplies the C's write primitives the three bodies also call - `undo_open`,
/// `record_node_tree` and the `SetAngles`/`SetScales` vtable writes (`:4693-4697`) - so this
/// trait declares only what is new to the mesh lane. `Chain`/`Nodes` supply the document identity
/// and the tree; the production implementation is [`GameMesh`], which forwards `Write`, `Chain`
/// and `Nodes` to the live Hammer object and binds each new method here to the same generated
/// vtable slot or pinned `HFN_*` entry the C calls.
pub(crate) trait Mesh: Write {
    /// `doc->m_pRoot` + `root->m_fileId` (`:5396-5397`): `None` is the C's "no document root".
    fn root_file_id(&self, doc: Doc) -> Option<u32>;
    /// `is_mesh` (`:3326`): `obj_is(node, PIN_HAMMER, HV_MAPMESH_VT)`.
    fn is_mesh(&self, node: Node) -> bool;
    /// `g_hfn.CreateMeshElement(fileId, name, srcFile, srcLine)` (`:5399`). `src` is
    /// `&'static CStr` because the C passes the literal `"mcp_bridge"`: the pointer is static
    /// storage Hammer may keep.
    fn create_mesh_element(
        &self,
        file_id: u32,
        name: u64,
        src: &'static CStr,
        line: i32,
    ) -> Option<Node>;
    /// `g_hfn.InsertNode(doc, node, 0)` (`:5403`).
    fn insert_node(&self, doc: Doc, node: Node);
    /// `g_hfn.RecordNode(hist, CMapNode_Undoable(node), 1)` (`:5404`).
    fn record_node(&self, hist: usize, node: Node);
    /// `CHammerPolyMesh`'s two component counts (`:4674`, `:4742-4743`): `(vertices, faces)`.
    /// The C reads them unguarded on an identity-checked mesh; so does the live impl.
    fn mesh_counts(&self, node: Node) -> (i32, i32);
    /// `CollectHandles(&hm->faces, &v)` (`:4680`, `:5460`): the face handles, or `None` when
    /// the call answered 0. `cap` is the C's buffer size (the mesh's face count).
    fn collect_faces(&self, node: Node, cap: usize) -> Option<Vec<MeshHandle>>;
    /// `MeshInitFromPolygons(mesh, &group, defmat, GEO_UV_MODE, &out)` (`:4717`): the created
    /// face handles (the C's `out.size` of them), `cap` the caller's `out.alloc`.
    fn init_from_polygons(
        &self,
        node: Node,
        polys: &[Vec<[f32; 3]>],
        defmat: &[u8],
        cap: usize,
    ) -> Vec<MeshHandle>;
    /// `MeshDeleteFaces(mesh, &v)` (`:4681`): `faces` is the collected list (`v.size`), `alloc`
    /// the C's `v.alloc` (the mesh's face count). The live impl hands Hammer a buffer of `alloc`
    /// handles with the collected ones at the front, exactly as the C's `geo_alloc(n)` does.
    fn delete_faces(&self, node: Node, faces: &[MeshHandle], alloc: usize);
    /// `AssignMaterialToFaces(hm, faces, count, material)` (`:4726`, `:5465`).
    fn assign_material(&self, node: Node, faces: &[MeshHandle], material: &[u8]);
    /// `node->m_Scales` (`:4691`): the C's unguarded read on an identity-checked mesh.
    fn read_scales(&self, node: Node) -> [f32; 3];
    /// `node->m_Angles` (`:4695`): the C's unguarded read.
    fn read_angles(&self, node: Node) -> [f32; 3];
    /// `node_parent` (`:3353-3358`, `mem_ok`-guarded): the node's parent, `None` when the field or
    /// the pointer is unreadable. `mesh.get` walks it to say whether the geometry is world-space.
    fn node_parent(&self, node: Node) -> Option<Node>;
    /// `node->vt->SetPivotPosition(node, pos)` (slot 140).
    fn set_pivot(&self, node: Node, value: [f32; 3]);
    // --- mesh.get's read primitives (`HFN_MESH_*`, re-report R1): all pure reads. -----------
    /// `CollectHandles(&hm->vertices, &v)`: the vertex handles in vertex order, or `None` when
    /// the call answered 0. `cap` is the buffer size (the mesh's vertex count).
    fn collect_vertices(&self, node: Node, cap: usize) -> Option<Vec<MeshHandle>>;
    /// `HFN_MESH_FACE_FIRST_EDGE`: the face's first half-edge ([`INVALID_HANDLE`] if stale).
    fn face_first_edge(&self, node: Node, face: &MeshHandle) -> MeshHandle;
    /// `HFN_MESH_EDGE_NEXT`: the next half-edge around the same face.
    fn edge_next(&self, node: Node, edge: &MeshHandle) -> MeshHandle;
    /// `HFN_MESH_EDGE_VERTEX`: the vertex the half-edge points TO.
    fn edge_vertex(&self, node: Node, edge: &MeshHandle) -> MeshHandle;
    /// `HFN_MESH_VERTEX_POSITIONS` with an identity matrix: the node-local positions of `verts`,
    /// one call for all (the ONE position source of `mesh.get`; handle/generation/data-index
    /// checked by Hammer, a failing vertex reads `(0,0,0)`). `None` = the mesh has no position
    /// stream (the function answered false): `mesh.get` replies an error, never a guess.
    fn vertex_positions(&self, node: Node, verts: &[MeshHandle]) -> Option<Vec<[f32; 3]>>;
    /// `HFN_MESH_FACE_MATERIAL_INDEX`: the face's material table index, -1 if none.
    /// Safe on Hammer's stream invariant: the material-index face stream is created on every
    /// creation/load path (its required-streams helper), and Hammer's own readers use it unchecked.
    fn face_material_index(&self, node: Node, face: &MeshHandle) -> i32;
    /// `HFN_MESH_MATERIAL_NAME`: the table entry's name, copied at once (the pointer is
    /// mesh-owned). Only called with an index `face_material_index` returned (>= 0).
    fn material_name(&self, node: Node, index: i32) -> Vec<u8>;
    // --- mesh.setFaceTexture's primitives (`HFN_MESH_*_FACE_TEXTURE`, WP1.5) ---------------
    /// `HFN_MESH_GET_FACE_TEXTURE`: the face's texture parameters for `set`. The outputs are
    /// pre-filled with NaN and the function has no result, so a NaN left in the answer means
    /// "not read" (a stale face, or a texture set the mesh does not have): see [`FaceTex::read`].
    fn face_texture(&self, node: Node, face: &MeshHandle, set: u32) -> FaceTex;
    /// `HFN_MESH_SET_FACE_TEXTURE` over one face: absolute axes, offsets and scale.
    fn set_face_texture(&self, node: Node, face: &MeshHandle, set: u32, tex: &FaceTex);
    /// `HFN_MESH_ALIGN_FACE_TEXTURE` over `faces` with an identity origin.
    fn align_face_textures(&self, node: Node, faces: &[MeshHandle], set: u32, mode: i32, flags: i32);
    /// `HFN_MESH_JUSTIFY_FACE_TEXTURE` over `faces` (per-face extents, no group bounds).
    fn justify_face_textures(&self, node: Node, faces: &[MeshHandle], set: u32, mode: u32);
}

/// The production mesh host: `ops::hammer`'s live writer (the same object as the write lane's
/// `&dyn Write`) plus the five bound mesh functions.
///
/// Constructed per body from [`game_write`], the same `&'static dyn Write` the read/write lanes
/// use, so the document tokens and the bind/fingerprint statics are the C's.
pub(crate) struct GameMesh(&'static dyn Write);

impl Chain for GameMesh {
    fn module_reason(&self) -> Option<&'static str> {
        self.0.module_reason()
    }
    fn app(&self) -> Option<usize> {
        self.0.app()
    }
    fn session(&self, app: usize) -> Option<usize> {
        self.0.session(app)
    }
    fn doc_session(&self, session: usize) -> Option<usize> {
        self.0.doc_session(session)
    }
    fn owner(&self, doc_session: usize) -> usize {
        self.0.owner(doc_session)
    }
    fn doc_of(&self, doc_session: usize) -> usize {
        self.0.doc_of(doc_session)
    }
    fn doc_backptr(&self, doc: usize) -> usize {
        self.0.doc_backptr(doc)
    }
    fn is_of(&self, object: usize, vtable: u32) -> bool {
        self.0.is_of(object, vtable)
    }
    fn root(&self, doc: usize) -> usize {
        self.0.root(doc)
    }
    fn active_flag(&self, doc: usize) -> bool {
        self.0.active_flag(doc)
    }
    fn selection(&self, doc: usize) -> usize {
        self.0.selection(doc)
    }
    fn scope_depth(&self, doc: usize) -> u32 {
        self.0.scope_depth(doc)
    }
    fn undo_history(&self, doc: usize) -> usize {
        self.0.undo_history(doc)
    }
    fn active_history(&self) -> usize {
        self.0.active_history()
    }
}

impl Nodes for GameMesh {
    fn children(&self, node: Node) -> Option<Vec<Node>> {
        self.0.children(node)
    }
    fn class(&self, node: Node) -> &'static str {
        self.0.class(node)
    }
    fn id(&self, node: Node) -> Option<i32> {
        self.0.id(node)
    }
    fn origin(&self, node: Node) -> Option<[f32; 3]> {
        self.0.origin(node)
    }
    fn mesh_slots(&self, node: Node) -> Option<(i32, i32)> {
        self.0.mesh_slots(node)
    }
    fn world(&self, doc: Doc) -> Option<Node> {
        self.0.world(doc)
    }
}

/// Forward every [`Write`] method to the live Hammer object this host wraps: the C's `g_hfn`
/// table is one object for reads and writes, so the mesh lane must not hold a second copy of
/// `undo_open`/`record_node_tree`/`SetAngles`/`SetScales` (`mcp_bridge.c:4693-4697`, `:5435`,
/// `:5464`).
macro_rules! forward_write {
    ($host:ty { $( fn $name:ident($($arg:ident: $argt:ty),*) -> $ret:ty; )* }) => {
        impl Write for $host {
            $(
                fn $name(&self, $($arg: $argt),*) -> $ret {
                    self.0.$name($($arg),*)
                }
            )*
        }
    };
}

forward_write!(GameMesh {
    fn class_exists(cls: &[u8]) -> bool;
    fn create_node(doc: Doc, target: &mut CreateTarget) -> Option<Node>;
    fn properties(node: Node) -> Option<usize>;
    fn undo_open(doc: Doc, label: &'static CStr) -> usize;
    fn record_node_tree(hist: usize, node: Node) -> ();
    fn doc_set_modified(doc: Doc) -> ();
    fn get_active_history() -> usize;
    fn get_redo_history(doc: Doc) -> usize;
    fn history_ok(hist: usize) -> bool;
    fn history_info(hist: usize) -> HistoryInfo;
    fn undo_pop(hist: usize) -> ();
    fn node_locked(node: Node) -> bool;
    fn delete_node(doc: Doc, node: Node) -> ();
    fn set_origin(node: Node, value: [f32; 3]) -> ();
    fn set_angles(node: Node, value: [f32; 3]) -> ();
    fn set_scales(node: Node, value: [f32; 3]) -> ();
    fn angles(node: Node) -> Option<[f32; 3]>;
    fn scales(node: Node) -> Option<[f32; 3]>;
    fn attr_previous(element: usize, name: &[u8]) -> PrevValue;
    fn attr_info(element: usize, name: &[u8]) -> AttrInfo;
    fn case_variant(element: usize, name: &[u8]) -> Option<Vec<u8>>;
    fn set_key_value(node: Node, name: &[u8], value: &[u8]) -> ();
    fn set_value_int(attr: usize, value: i32) -> ();
    fn set_value_float(attr: usize, value: f32) -> ();
    fn conn_count(node: Node) -> Option<usize>;
    fn conn_entry(node: Node, index: usize) -> Option<Conn>;
    fn conn_filter_fields(node: Node, index: usize) -> Option<FilterFields>;
    fn add_connection(node: Node, spec: &ConnSpec) -> (bool, Option<i32>);
    fn remove_connection(node: Node, index: i32) -> bool;
});

/// The production mesh host (`cfg(windows)`), or the loud non-Windows refusal.
pub(crate) fn host() -> GameMesh {
    GameMesh(game_write())
}

// ---------------------------------------------------------------------------------------------
// The mesh reset / build (`mcp_bridge.c:4671-4765`)
// ---------------------------------------------------------------------------------------------

/// The C's `S2HandleVec` buffer for `MeshDeleteFaces` (`mcp_bridge.c:4677-4681`): `alloc`
/// zeroed handles with the collected `faces` copied to the front, and the C's `v.size` (the
/// collected count, which never exceeds `alloc`).
///
/// The C allocates `sizeof(S2MeshHandle) * n` with `v.alloc = n` *before* `CollectHandles` runs,
/// so `MeshDeleteFaces` is handed the full `n`-handle block even when fewer were collected. A
/// buffer sized to the collected count instead would let Hammer write past its end.
#[allow(dead_code)] // the Windows live impl and the host tests are the callers
pub(crate) fn delete_buffer(faces: &[MeshHandle], alloc: usize) -> (Vec<MeshHandle>, i32) {
    let size = faces.len().min(alloc);
    let mut handles = vec![[0u8; 16]; alloc];
    handles[..size].copy_from_slice(&faces[..size]);
    (handles, size as i32)
}

/// `geo_reset_mesh` (`mcp_bridge.c:4671-4701`): empty an existing mesh and put it back on an
/// identity transform so `InitializeFromPolygonGroup` can take world points.
///
/// Order is the C's: delete every face (through `CollectHandles` + `MeshDeleteFaces`), then the
/// m2.18 fix's scales `{1,1,1}` (slot 19) and angles `{0,0,0}` (slot 18) - each only when the
/// field is not already at that value - and always the pivot `{0,0,0}` (slot 140).
fn geo_reset_mesh(host: &dyn Mesh, mesh: Node) -> bool {
    let (_, faces) = host.mesh_counts(mesh); // `hm->faces.count` (`:4674`)
    if faces > 0 {
        if faces > GEO_FACE_CAP {
            return false; // :4676
        }
        let Some(handles) = host.collect_faces(mesh, faces as usize) else {
            return false; // :4680
        };
        if !handles.is_empty() {
            host.delete_faces(mesh, &handles, faces as usize); // :4681
        }
    }
    // :4691-4698: the m2.18 fix (re-props.md Q2): scales then angles, Hammer's own clear order.
    if host.read_scales(mesh) != [1.0, 1.0, 1.0] {
        host.set_scales(mesh, [1.0, 1.0, 1.0]);
    }
    if host.read_angles(mesh) != [0.0, 0.0, 0.0] {
        host.set_angles(mesh, [0.0, 0.0, 0.0]);
    }
    host.set_pivot(mesh, [0.0, 0.0, 0.0]); // :4699-4700
    true
}

/// `geo_build` (`mcp_bridge.c:4705-4735`): build the polygons, run `MeshInitFromPolygons` and
/// assign the per-face materials. Answers the created face count (`out.size`).
fn geo_build(host: &dyn Mesh, mesh: Node, g: &GGeom) -> Result<i32, &'static str> {
    // :4706-4713: the recs and the flat point array, exactly as the C fills them
    // (`pts[fstart[f] + k] = v[idx[fstart[f] + k]]`).
    let mut polys: Vec<Vec<[f32; 3]>> = Vec::with_capacity(g.nf);
    for f in 0..g.nf {
        let start = g.fstart[f] as usize;
        let count = g.fcount[f] as usize;
        let mut poly = Vec::with_capacity(count);
        for k in 0..count {
            poly.push(g.v[g.idx[start + k] as usize]);
        }
        polys.push(poly);
    }
    // :4715-4718
    let faces = host.init_from_polygons(mesh, &polys, &g.defmat, g.nf);
    let count = faces.len() as i32; // `*facesOut = out.size`
    if g.nmat != 0 && faces.len() == g.nf {
        // :4719-4728: one AssignMaterialToFaces per distinct material slot, in slot order.
        for mi in 0..g.nmat {
            let sel: Vec<MeshHandle> = (0..g.nf)
                .filter(|&f| g.fmat[f] == mi as i32)
                .map(|f| faces[f])
                .collect();
            if !sel.is_empty() {
                host.assign_material(mesh, &sel, &g.mats[mi]);
            }
        }
    }
    if faces.is_empty() {
        return Err("Hammer created no faces (degenerate polygons?)"); // :4732-4733
    }
    Ok(count)
}

/// One mesh's identity and geometry, read for the reply (`geo_mesh_summary`, `:4736-4746`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct MeshSummary {
    /// `nodeID` (`-1` when `node_id_of` failed, as the C's initialiser).
    pub node_id: i32,
    /// `origin` (zeros when `node_origin` failed, as the C's initialiser).
    pub origin: [f32; 3],
    /// `hm->vertices.count`.
    pub vertex_slots: i32,
    /// `hm->faces.count`.
    pub face_slots: i32,
}

/// `geo_mesh_summary`'s reads (`:4738-4743`), in the C's order: `node_id_of`,
/// `node_origin`, then the two component counts.
fn mesh_summary(host: &dyn Mesh, mesh: Node) -> MeshSummary {
    let node_id = host.id(mesh).unwrap_or(-1);
    let origin = host.origin(mesh).unwrap_or([0.0, 0.0, 0.0]);
    let (vertex_slots, face_slots) = host.mesh_counts(mesh);
    MeshSummary {
        node_id,
        origin,
        vertex_slots,
        face_slots,
    }
}

// ---------------------------------------------------------------------------------------------
// The result builders (`geo_mesh_summary` + the three reply tails)
// ---------------------------------------------------------------------------------------------

/// `geo_mesh_summary` (`mcp_bridge.c:4736-4746`): `"nodeID":N,"class":"mesh","origin":[...],
/// "vertexSlots":N,"faceSlots":N`.
pub(crate) fn mesh_summary_into(out: &mut Sb, summary: &MeshSummary) {
    out.add(b"\"nodeID\":");
    out.addi(i64::from(summary.node_id));
    out.add(b",\"class\":\"mesh\",\"origin\":");
    addf3(out, summary.origin);
    out.add(b",\"vertexSlots\":");
    out.addi(i64::from(summary.vertex_slots));
    out.add(b",\"faceSlots\":");
    out.addi(i64::from(summary.face_slots));
}

/// `mesh.create`'s result (`mcp_bridge.c:5407-5415`), byte for byte.
pub(crate) fn create_reply_into(
    out: &mut Sb,
    summary: &MeshSummary,
    input_vertices: i64,
    faces: i64,
    materials: i64,
) {
    out.add(b"{");
    mesh_summary_into(out, summary);
    out.add(b",\"inputVertices\":");
    out.addi(input_vertices);
    out.add(b",\"faces\":");
    out.addi(faces);
    out.add(b",\"materials\":");
    out.addi(materials);
    out.add(b",\"undoLabel\":\"MCP: create mesh\"}");
}

/// `mesh.setGeometry`'s result (`mcp_bridge.c:5438-5446`), byte for byte.
pub(crate) fn set_geometry_reply_into(
    out: &mut Sb,
    summary: &MeshSummary,
    input_vertices: i64,
    faces: i64,
) {
    out.add(b"{");
    mesh_summary_into(out, summary);
    out.add(b",\"inputVertices\":");
    out.addi(input_vertices);
    out.add(b",\"faces\":");
    out.addi(faces);
    out.add(b",\"undoLabel\":\"MCP: set mesh geometry\"}");
}

/// `mesh.setMaterial`'s result (`mcp_bridge.c:5467-5472`), byte for byte.
pub(crate) fn set_material_reply_into(
    out: &mut Sb,
    summary: &MeshSummary,
    faces: i64,
    material: &[u8],
) {
    out.add(b"{");
    mesh_summary_into(out, summary);
    out.add(b",\"faces\":");
    out.addi(faces);
    out.add(b",\"material\":");
    out.addjson(material);
    out.add(b",\"undoLabel\":\"MCP: set mesh material\"}");
}

// ---------------------------------------------------------------------------------------------
// The three ops (`mcp_bridge.c:5392-5475`)
// ---------------------------------------------------------------------------------------------

/// `geo_target_mesh` (`mcp_bridge.c:5416-5426`): the `nodeID` form, the live node and its mesh
/// identity - all before the params are parsed. The C's params pointer is returned so the
/// caller can parse the rest without re-reading it.
fn geo_target_mesh<'a>(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&'a str>,
) -> Result<(Node, &'a str), OpErr> {
    let Some(params) = params else {
        // :5417-5420
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "nodeID: a non-negative int up to 2147483647 is required",
        ));
    };
    let id = json_int_top(params.as_bytes(), b"nodeID");
    let Some(id) = id else {
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
        ));
    }
    let Some(node) = geo_find(host, doc, id as i32) else {
        return Err(OpErr::new(
            -32022,
            "NodeNotFound",
            "no node with this nodeID in the live document",
        )); // :5422
    };
    if !host.is_mesh(node) {
        return Err(OpErr::new(-32602, "InvalidParams", "node is not a mesh")); // :5423-5424
    }
    Ok((node, params))
}

/// `mesh.create` (`mcp_bridge.c:5392-5415`).
pub(crate) fn op_mesh_create(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    // :5393-5395
    let Some(params) = params else {
        return Err(OpErr::new(-32602, "InvalidParams", "params"));
    };
    let g = geom_parse(params.as_bytes())
        .map_err(|why| OpErr::new(-32602, "InvalidParams", why))?;
    // :5396-5397
    let Some(file_id) = host.root_file_id(doc) else {
        return Err(OpErr::new(-32603, "InternalError", "no document root"));
    };
    // :5398: the group opens before the element exists (the C's order).
    let hist = host.undo_open(doc, c"MCP: create mesh");
    // :5399-5400
    let mesh = match host.create_mesh_element(file_id, 0, c"mcp_bridge", 0) {
        Some(mesh) if host.is_mesh(mesh) => mesh,
        _ => {
            return Err(OpErr::new(
                -32603,
                "InternalError",
                "CreateElement<CMapMesh> failed",
            ))
        }
    };
    // :5401-5402
    let faces = geo_build(host, mesh, &g)
        .map_err(|why| OpErr::new(-32603, "InternalError", why))?;
    // :5403-5404
    host.insert_node(doc, mesh);
    host.record_node(hist, mesh);
    log::line("mesh.create: ok"); // :5405
    // :5406-5415
    let summary = mesh_summary(host, mesh);
    create_reply_into(
        out,
        &summary,
        g.nv as i64,
        i64::from(faces),
        i64::from(g.nmat as i32) + 1,
    );
    Ok(())
}

/// `mesh.setGeometry` (`mcp_bridge.c:5428-5447`).
pub(crate) fn op_mesh_set_geometry(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (mesh, params) = geo_target_mesh(host, doc, params)?; // :5429-5430
    let g = geom_parse(params.as_bytes())
        .map_err(|why| OpErr::new(-32602, "InvalidParams", why))?; // :5431-5433
    // :5434-5435
    let hist = host.undo_open(doc, c"MCP: set mesh geometry");
    host.record_node_tree(hist, mesh);
    if !geo_reset_mesh(host, mesh) {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "could not clear the mesh",
        )); // :5436
    }
    let faces = geo_build(host, mesh, &g)
        .map_err(|why| OpErr::new(-32603, "InternalError", why))?; // :5437
    log::line("mesh.setGeometry: ok"); // :5438
    let summary = mesh_summary(host, mesh);
    set_geometry_reply_into(out, &summary, g.nv as i64, i64::from(faces));
    Ok(())
}

/// `mesh.setMaterial` (`mcp_bridge.c:5449-5474`).
pub(crate) fn op_mesh_set_material(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (mesh, params) = geo_target_mesh(host, doc, params)?; // :5450-5451
    // :5452-5454
    let mut material = [0u8; GEO_MAT_CAP];
    if !json_string(params.as_bytes(), b"material", &mut material) || material[0] == 0 {
        return Err(OpErr::new(-32602, "InvalidParams", "material required"));
    }
    let material = cstr(&material).to_vec();
    // :5455-5457
    let (_, faces) = host.mesh_counts(mesh);
    let n = faces;
    if n <= 0 || n > GEO_FACE_CAP {
        return Err(OpErr::new(-32603, "InternalError", "mesh has no faces"));
    }
    // :5457-5462
    let Some(handles) = host.collect_faces(mesh, n as usize) else {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "could not collect face handles",
        ));
    };
    if handles.is_empty() {
        return Err(OpErr::new(
            -32603,
            "InternalError",
            "could not collect face handles",
        ));
    }
    // :5463-5465
    let hist = host.undo_open(doc, c"MCP: set mesh material");
    host.record_node_tree(hist, mesh);
    host.assign_material(mesh, &handles, &material);
    // :5466-5473
    let summary = mesh_summary(host, mesh);
    set_material_reply_into(out, &summary, handles.len() as i64, &material);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// mesh.setFaceTexture (bridge m3.14, WP1.5): per-face texture alignment
// ---------------------------------------------------------------------------------------------

/// The identity origin `HFN_MESH_ALIGN_FACE_TEXTURE` takes (3 rows of 4 floats): the texture grid
/// starts at the world origin. Data, not an address.
#[allow(dead_code)] // the Windows live impl is the caller
pub(crate) static IDENTITY_3X4: [f32; 12] =
    [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0];
/// `flags` for the align function: force (1) | new axes (2) | zero offsets (4) | default scale (8).
const ALIGN_FLAGS_ALL: i32 = 15;
/// The same without the default-scale reset (`keepScale`).
const ALIGN_FLAGS_KEEP_SCALE: i32 = 7;
/// How many faces' read-back one reply carries (the rest are applied, the reply says so).
const TEX_READBACK_CAP: usize = 1000;
/// Hammer's UV sets that are single sets (3 means "all", which this method does not offer).
const TEX_SET_MAX: u32 = 1;
/// Most faces one `mode "axes"` call lists (m3.18). Each face is one `HFN_MESH_SET_FACE_TEXTURE`
/// call plus two reads inside one undo group; the params line is far below the IPC line cap
/// long before this.
pub(crate) const TEX_AXES_MAX_FACES: usize = 65_536;
/// `|u x v| <= TEX_PARALLEL_EPS * |u| * |v|` counts as parallel (sin of the angle between).
const TEX_PARALLEL_EPS: f64 = 1e-6;

/// One face's texture parameters (`textureAxisU/V:set` Vector4 each, `textureScale:set` Vector2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FaceTex {
    /// `textureAxisU`: xyz axis, w the offset in texels.
    pub u: [f32; 4],
    /// `textureAxisV`.
    pub v: [f32; 4],
    /// `textureScale`: world units per texel.
    pub scale: [f32; 2],
}

impl FaceTex {
    /// The NaN pre-fill the getter is called with.
    #[allow(dead_code)] // the Windows live impl is the caller
    pub(crate) fn unread() -> Self {
        Self { u: [f32::NAN; 4], v: [f32::NAN; 4], scale: [f32::NAN; 2] }
    }

    /// Bit-for-bit equality (what a write followed by a read must give).
    pub(crate) fn same_bits(&self, other: &Self) -> bool {
        let bits = |t: &Self| -> Vec<u32> {
            t.u.iter().chain(&t.v).chain(&t.scale).map(|x| x.to_bits()).collect()
        };
        bits(self) == bits(other)
    }

    /// Every component finite: the getter wrote all three outputs.
    pub(crate) fn is_read(&self) -> bool {
        self.u.iter().chain(&self.v).chain(&self.scale).all(|x| x.is_finite())
    }
}

/// What `mode` asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TexMode {
    /// Align to the world grid (`AlignWorld`, align mode 1).
    World,
    /// Align to the face plane (`AlignFace`, align mode 2).
    Face,
    /// Justify with Hammer's mode number (top 1, bottom 2, left 3, center 4, right 5, fit 6,
    /// fitU 7, fitV 8).
    Justify(u32),
    /// Only the shift/scale/rotate fields.
    Set,
    /// Absolute per-face axes: `faces` is a list of `{index, axisU, axisV, scale}` objects
    /// (bridge m3.18). Nothing else is computed.
    Axes,
}

/// The validated `mesh.setFaceTexture` params.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TexParams {
    pub faces: Option<Vec<usize>>,
    /// `mode "axes"` only: the absolute texture parameters of `faces[k]` (same length as
    /// `faces`; empty in every other mode).
    pub axes: Vec<FaceTex>,
    pub mode: TexMode,
    pub keep_scale: bool,
    pub shift: Option<[f32; 2]>,
    pub scale: Option<[f32; 2]>,
    pub rotate: Option<f32>,
    pub tex_set: u32,
}

fn tex_pair(v: &serde_json::Value, name: &str) -> Result<[f32; 2], String> {
    let bad = || format!("{name}: [u, v] of two finite numbers required");
    let a = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
    let mut out = [0f32; 2];
    for (o, x) in out.iter_mut().zip(a) {
        let f = x.as_f64().ok_or_else(bad)? as f32;
        if !f.is_finite() {
            return Err(bad());
        }
        *o = f;
    }
    Ok(out)
}

/// Parse and fully validate the params (everything that does not need the mesh), in a fixed
/// order. The `why` text is the `-32602` reason.
pub(crate) fn tex_parse(params: &str) -> Result<TexParams, String> {
    let root: serde_json::Value =
        serde_json::from_str(params).map_err(|e| format!("params: not valid JSON ({e})"))?;
    let obj = root.as_object().ok_or("params: an object is required")?;
    let mode_name = obj.get("mode").and_then(|m| m.as_str());
    let justify = match obj.get("justify") {
        None | Some(serde_json::Value::Null) => None,
        Some(j) => Some(j.as_str().ok_or("justify: a string is required")?),
    };
    let mode = match mode_name {
        Some("world") => TexMode::World,
        Some("face") => TexMode::Face,
        Some("set") => TexMode::Set,
        Some("axes") => TexMode::Axes,
        Some("justify") => {
            let j = justify.ok_or("justify: required when mode is \"justify\"")?;
            TexMode::Justify(match j {
                "top" => 1,
                "bottom" => 2,
                "left" => 3,
                "center" => 4,
                "right" => 5,
                "fit" => 6,
                "fitU" => 7,
                "fitV" => 8,
                _ => {
                    return Err("justify: one of left, right, top, bottom, center, fit, fitU, fitV"
                        .to_string())
                }
            })
        }
        _ => return Err("mode: one of world, face, justify, set, axes is required".to_string()),
    };
    if justify.is_some() && !matches!(mode, TexMode::Justify(_)) {
        return Err("justify: only valid with mode \"justify\"".to_string());
    }
    let keep_scale = match obj.get("keepScale") {
        None | Some(serde_json::Value::Null) => false,
        Some(b) => b.as_bool().ok_or("keepScale: a boolean is required")?,
    };
    if keep_scale && !matches!(mode, TexMode::World | TexMode::Face) {
        return Err("keepScale: only valid with mode \"world\" or \"face\"".to_string());
    }
    if mode == TexMode::Axes {
        return tex_parse_axes(obj);
    }
    let faces = match obj.get("faces") {
        None | Some(serde_json::Value::Null) => None,
        Some(f) => {
            let a = f.as_array().ok_or("faces: an array of face indices is required")?;
            if a.is_empty() {
                return Err("faces: at least one index (omit the member for every face)".to_string());
            }
            let mut ix = Vec::with_capacity(a.len());
            for (k, x) in a.iter().enumerate() {
                let i = x
                    .as_u64()
                    .filter(|i| *i <= u64::from(u32::MAX))
                    .ok_or_else(|| format!("faces[{k}]: a non-negative integer is required"))?
                    as usize;
                if ix.contains(&i) {
                    return Err(format!("faces[{k}]: index {i} is listed twice"));
                }
                ix.push(i);
            }
            Some(ix)
        }
    };
    let opt_pair = |name: &str| -> Result<Option<[f32; 2]>, String> {
        match obj.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(v) => tex_pair(v, name).map(Some),
        }
    };
    let shift = opt_pair("shift")?;
    let scale = opt_pair("scale")?;
    if let Some(s) = scale {
        if s[0] <= 0.0 || s[1] <= 0.0 {
            return Err("scale: both components must be > 0 (world units per texel)".to_string());
        }
    }
    let rotate = match obj.get("rotate") {
        None | Some(serde_json::Value::Null) => None,
        Some(r) => {
            let f = r.as_f64().ok_or("rotate: a number of degrees is required")?;
            if !f.is_finite() || f.abs() > 36_000.0 {
                return Err("rotate: degrees, finite, |value| <= 36000".to_string());
            }
            Some(f as f32)
        }
    };
    let tex_set = match obj.get("texSet") {
        None | Some(serde_json::Value::Null) => 0,
        Some(t) => t
            .as_u64()
            .filter(|t| *t <= u64::from(TEX_SET_MAX))
            .ok_or("texSet: 0 or 1")? as u32,
    };
    if matches!(mode, TexMode::Justify(_)) && (shift.is_some() || scale.is_some() || rotate.is_some()) {
        // The explicit fields are applied after the mode and would overwrite what justify fitted.
        return Err("shift, scale and rotate are not valid with mode \"justify\" (they would undo the \
                    justify): justify first, then call again with mode \"set\""
            .to_string());
    }
    if mode == TexMode::Set && shift.is_none() && scale.is_none() && rotate.is_none() {
        return Err("mode \"set\" needs at least one of shift, scale, rotate".to_string());
    }
    Ok(TexParams { faces, axes: Vec::new(), mode, keep_scale, shift, scale, rotate, tex_set })
}

/// A `[x, y, z, offset]` of four finite numbers.
fn tex_quad(v: &serde_json::Value, name: &str) -> Result<[f32; 4], String> {
    let bad = || format!("{name}: [x, y, z, offset] of four finite numbers required");
    let a = v.as_array().filter(|a| a.len() == 4).ok_or_else(bad)?;
    let mut out = [0f32; 4];
    for (o, x) in out.iter_mut().zip(a) {
        let f = x.as_f64().ok_or_else(bad)? as f32;
        if !f.is_finite() {
            return Err(bad());
        }
        *o = f;
    }
    Ok(out)
}

/// `mode "axes"`: `faces` is `[{index, axisU, axisV, scale}, ...]`, `texSet` optional, and none of
/// `justify`, `keepScale`, `shift`, `scale`, `rotate` (they mean something else in the other modes
/// and would silently do nothing here).
fn tex_parse_axes(obj: &serde_json::Map<String, serde_json::Value>) -> Result<TexParams, String> {
    for name in ["justify", "keepScale", "shift", "scale", "rotate"] {
        if !matches!(obj.get(name), None | Some(serde_json::Value::Null)) {
            return Err(format!(
                "{name}: not valid with mode \"axes\" (put axisU, axisV and scale inside each faces entry)"
            ));
        }
    }
    let list = obj
        .get("faces")
        .and_then(|f| f.as_array())
        .ok_or("faces: an array of {index, axisU, axisV, scale} is required with mode \"axes\"")?;
    if list.is_empty() {
        return Err("faces: at least one {index, axisU, axisV, scale} entry".to_string());
    }
    if list.len() > TEX_AXES_MAX_FACES {
        return Err(format!("faces: at most {TEX_AXES_MAX_FACES} entries per call"));
    }
    let mut faces = Vec::with_capacity(list.len());
    let mut axes = Vec::with_capacity(list.len());
    for (k, entry) in list.iter().enumerate() {
        let e = entry
            .as_object()
            .ok_or_else(|| format!("faces[{k}]: an object {{index, axisU, axisV, scale}} is required"))?;
        let index = e
            .get("index")
            .and_then(|i| i.as_u64())
            .filter(|i| *i <= u64::from(u32::MAX))
            .ok_or_else(|| format!("faces[{k}].index: a non-negative integer is required"))?
            as usize;
        if faces.contains(&index) {
            return Err(format!("faces[{k}]: index {index} is listed twice"));
        }
        let need = |name: &str| e.get(name).ok_or_else(|| format!("faces[{k}].{name}: required"));
        let u = tex_quad(need("axisU")?, &format!("faces[{k}].axisU"))?;
        let v = tex_quad(need("axisV")?, &format!("faces[{k}].axisV"))?;
        let scale = tex_pair(need("scale")?, &format!("faces[{k}].scale"))?;
        if scale[0] <= 0.0 || scale[1] <= 0.0 {
            return Err(format!(
                "faces[{k}].scale: both components must be > 0 (world units per texel)"
            ));
        }
        let (a, b) = ([f64::from(u[0]), f64::from(u[1]), f64::from(u[2])], [f64::from(v[0]), f64::from(v[1]), f64::from(v[2])]);
        let len = |x: [f64; 3]| (x[0] * x[0] + x[1] * x[1] + x[2] * x[2]).sqrt();
        let (la, lb) = (len(a), len(b));
        if la <= 0.0 || !la.is_finite() {
            return Err(format!("faces[{k}].axisU: the xyz axis must not be zero-length"));
        }
        if lb <= 0.0 || !lb.is_finite() {
            return Err(format!("faces[{k}].axisV: the xyz axis must not be zero-length"));
        }
        let cross = [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
        if len(cross) <= TEX_PARALLEL_EPS * la * lb {
            return Err(format!("faces[{k}]: axisU and axisV must not be parallel"));
        }
        faces.push(index);
        axes.push(FaceTex { u, v, scale });
    }
    let tex_set = match obj.get("texSet") {
        None | Some(serde_json::Value::Null) => 0,
        Some(t) => t
            .as_u64()
            .filter(|t| *t <= u64::from(TEX_SET_MAX))
            .ok_or("texSet: 0 or 1")? as u32,
    };
    Ok(TexParams {
        faces: Some(faces),
        axes,
        mode: TexMode::Axes,
        keep_scale: false,
        shift: None,
        scale: None,
        rotate: None,
        tex_set,
    })
}

/// Apply `rotate` (about the face normal, added to the current orientation), then `scale`, then
/// `shift` to one face's read-back: `U' = cos*U + sin*V`, `V' = -sin*U + cos*V` on the axis xyz,
/// offsets kept unless `shift` overrides them.
pub(crate) fn tex_adjust(
    tex: &FaceTex,
    shift: Option<[f32; 2]>,
    scale: Option<[f32; 2]>,
    rotate: Option<f32>,
) -> FaceTex {
    let mut out = *tex;
    if let Some(deg) = rotate {
        let r = f64::from(deg).to_radians();
        // Exact for the right angles a caller asks for (cos 90 deg is 6e-17 in f64).
        let snap = |x: f64| if x.abs() < 1e-12 { 0.0 } else { x };
        let (c, s) = (snap(r.cos()), snap(r.sin()));
        for k in 0..3 {
            let (u, v) = (f64::from(tex.u[k]), f64::from(tex.v[k]));
            // `+ 0.0` turns a -0 into +0 so the read-back never says "-0".
            out.u[k] = (c * u + s * v + 0.0) as f32;
            out.v[k] = (-s * u + c * v + 0.0) as f32;
        }
    }
    if let Some(s) = scale {
        out.scale = s;
    }
    if let Some(sh) = shift {
        out.u[3] = sh[0];
        out.v[3] = sh[1];
    }
    out
}

fn add_floats(out: &mut Sb, v: &[f32]) {
    out.add(b"[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        addf_exact(out, *x);
    }
    out.add(b"]");
}

/// `mesh.setFaceTexture`'s result: the mesh summary, the mode, how many faces were touched and
/// the per-face read-back (at most [`TEX_READBACK_CAP`] of them).
pub(crate) fn tex_reply_into(
    out: &mut Sb,
    summary: &MeshSummary,
    mode: &str,
    tex_set: u32,
    touched: usize,
    rows: &[(usize, FaceTex)],
    exact: Option<bool>,
) {
    out.add(b"{");
    mesh_summary_into(out, summary);
    out.add(b",\"mode\":");
    out.addjson(mode.as_bytes());
    out.add(b",\"texSet\":");
    out.addnum(u64::from(tex_set));
    out.add(b",\"touched\":");
    out.addnum(touched as u64);
    out.add(b",\"readBackTruncated\":");
    out.add(if rows.len() < touched { b"true" } else { b"false" });
    if let Some(exact) = exact {
        // mode "axes": did every face read back bit-for-bit what was asked?
        out.add(b",\"exact\":");
        out.add(if exact { b"true" } else { b"false" });
    }
    out.add(b",\"faces\":[");
    for (i, (index, tex)) in rows.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.add(b"{\"index\":");
        out.addnum(*index as u64);
        out.add(b",\"axisU\":");
        add_floats(out, &tex.u);
        out.add(b",\"axisV\":");
        add_floats(out, &tex.v);
        out.add(b",\"scale\":");
        add_floats(out, &tex.scale);
        out.add(b"}");
    }
    out.add(b"],\"undoLabel\":\"MCP: set face texture\"}");
}

/// The write landed but the read-back disagrees with what was written / asked for.
fn tex_mismatch(index: usize, what: &str) -> OpErr {
    OpErr::maybe(-32603, "TextureWriteMismatch", &format!("face {index}: {what}"))
}

/// A face (or texture set) that could not be read: the getter left its NaN pre-fill.
fn tex_unread(index: usize, set: u32, after_write: bool) -> OpErr {
    let why = format!(
        "face {index}: the texture parameters of set {set} could not be read (the mesh has no \
         such texture set, or the face is stale)"
    );
    if after_write {
        OpErr::maybe(-32603, "TextureSetUnknown", &why)
    } else {
        OpErr::new(-32603, "TextureSetUnknown", &why)
    }
}

/// `mesh.setFaceTexture`: align to the world grid / face plane, justify, and/or set shift, scale
/// and rotation on the chosen faces of one mesh, in one undo entry; or (`mode "axes"`, m3.18)
/// write absolute per-face axes.
pub(crate) fn op_mesh_set_face_texture(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (mesh, params) = geo_target_mesh(host, doc, params)?;
    let p = tex_parse(params).map_err(|why| OpErr::new(-32602, "InvalidParams", &why))?;
    let (_, n) = host.mesh_counts(mesh);
    if n <= 0 || n > GEO_FACE_CAP {
        return Err(OpErr::new(-32603, "InternalError", "mesh has no faces"));
    }
    let all = match host.collect_faces(mesh, n as usize) {
        Some(all) if !all.is_empty() => all,
        _ => {
            return Err(OpErr::new(
                -32603,
                "InternalError",
                "could not collect face handles",
            ))
        }
    };
    // The faces, in request order, by get_mesh index.
    let indices: Vec<usize> = match &p.faces {
        None => (0..all.len()).collect(),
        Some(ix) => {
            if let Some((k, i)) = ix.iter().enumerate().find(|(_, i)| **i >= all.len()) {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    &format!("faces[{k}]: index {i} out of range (the mesh has {} faces)", all.len()),
                ));
            }
            ix.clone()
        }
    };
    let handles: Vec<MeshHandle> = indices.iter().map(|&i| all[i]).collect();
    // Readable before anything is written: a texture set the mesh lacks is a silent no-op in the
    // game and a NaN here.
    for (&i, h) in indices.iter().zip(&handles) {
        if !host.face_texture(mesh, h, p.tex_set).is_read() {
            return Err(tex_unread(i, p.tex_set, false));
        }
    }
    let hist = host.undo_open(doc, c"MCP: set face texture");
    host.record_node_tree(hist, mesh);
    let mode_name = match p.mode {
        TexMode::World => {
            let flags = if p.keep_scale { ALIGN_FLAGS_KEEP_SCALE } else { ALIGN_FLAGS_ALL };
            host.align_face_textures(mesh, &handles, p.tex_set, 1, flags);
            "world"
        }
        TexMode::Face => {
            let flags = if p.keep_scale { ALIGN_FLAGS_KEEP_SCALE } else { ALIGN_FLAGS_ALL };
            host.align_face_textures(mesh, &handles, p.tex_set, 2, flags);
            "face"
        }
        TexMode::Justify(m) => {
            host.justify_face_textures(mesh, &handles, p.tex_set, m);
            "justify"
        }
        TexMode::Set => "set",
        TexMode::Axes => {
            // Absolute axes, one face per call (the setter writes ONE triple into every face it
            // gets), all inside the one undo entry opened above. A write that did not land is
            // not an error here: the read-back below is the reply and `exact` says whether it
            // matches the request.
            for ((&i, h), want) in indices.iter().zip(&handles).zip(&p.axes) {
                host.set_face_texture(mesh, h, p.tex_set, want);
                if !host.face_texture(mesh, h, p.tex_set).is_read() {
                    return Err(tex_unread(i, p.tex_set, true));
                }
            }
            "axes"
        }
    };
    if p.shift.is_some() || p.scale.is_some() || p.rotate.is_some() {
        // Per face: each has its own axes, and the setter writes one triple to every face it gets.
        for (&i, h) in indices.iter().zip(&handles) {
            let cur = host.face_texture(mesh, h, p.tex_set);
            if !cur.is_read() {
                return Err(tex_unread(i, p.tex_set, true));
            }
            let next = tex_adjust(&cur, p.shift, p.scale, p.rotate);
            host.set_face_texture(mesh, h, p.tex_set, &next);
            let now = host.face_texture(mesh, h, p.tex_set);
            if !now.same_bits(&next) {
                return Err(tex_mismatch(i, "the read-back differs from the values written"));
            }
        }
    }
    // Every touched face is read back and verified (only the reply rows are capped).
    let zero_offsets = matches!(p.mode, TexMode::World | TexMode::Face) && p.shift.is_none();
    let mut rows = Vec::with_capacity(indices.len().min(TEX_READBACK_CAP));
    let mut exact = (p.mode == TexMode::Axes).then_some(true);
    for (k, (&i, h)) in indices.iter().zip(&handles).enumerate() {
        let tex = host.face_texture(mesh, h, p.tex_set);
        if !tex.is_read() {
            return Err(tex_unread(i, p.tex_set, true));
        }
        if zero_offsets && (tex.u[3] != 0.0 || tex.v[3] != 0.0) {
            return Err(tex_mismatch(i, "the align left a non-zero texture offset (flag 4 should zero it)"));
        }
        if p.mode == TexMode::Axes && !tex.same_bits(&p.axes[k]) {
            exact = Some(false);
        }
        if rows.len() < TEX_READBACK_CAP {
            rows.push((i, tex));
        }
    }
    log::line("mesh.setFaceTexture: ok");
    let summary = mesh_summary(host, mesh);
    tex_reply_into(out, &summary, mode_name, p.tex_set, indices.len(), &rows, exact);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The dispatch arm and the handlers (`geo_dispatch`, `mcp_bridge.c:5612-5659`)
// ---------------------------------------------------------------------------------------------

/// The C's `GeoOp` (`mcp_bridge.c:5611`) over the [`Mesh`] host.
type GeoOp = fn(&dyn Mesh, Doc, Option<&str>, &mut Sb) -> Result<(), OpErr>;

/// The overflow text of a `geo_dispatch` result (`mcp_bridge.c:5654-5657`).
const GEO_OVERFLOW: &str = "response too large (use limit)";

/// `geo_dispatch` (`mcp_bridge.c:5612-5659`) for a mutating mesh op: the active document, the
/// full editable set, the caller's `expectDoc`, then the body, then the `doc` tail, then the
/// overflow check. The C's order is the gate's (a document that cannot be edited answers
/// `-32018` before the params are looked at). The tail-and-overflow half is
/// [`write_finish`], the same helper the write lane uses.
pub(crate) fn geo_dispatch(
    host: &dyn Mesh,
    id: &str,
    params: Option<&str>,
    out: &mut Sb,
    op: GeoOp,
) -> String {
    let outcome = write_gate(host, params.map(str::as_bytes))
        .and_then(|doc| op(host, doc, params, out).map(|()| (doc, false)));
    write_finish(id, outcome, out, GEO_OVERFLOW)
}

/// One handler: the C's 512 KB result buffer, `geo_dispatch`, then the envelope.
fn geo_handler(req: &Request, op: GeoOp) -> String {
    let mut buf = vec![0u8; OP_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    geo_dispatch(&host(), &req.id, req.params.as_deref(), &mut out, op)
}

/// `mesh.create`'s arm (`mcp_bridge.c:5972-5973`).
fn mesh_create(req: &Request) -> String {
    geo_handler(req, op_mesh_create)
}

/// `mesh.setGeometry`'s arm (`mcp_bridge.c:5974-5975`).
fn mesh_set_geometry(req: &Request) -> String {
    geo_handler(req, op_mesh_set_geometry)
}

/// `mesh.setFaceTexture`'s arm (bridge m3.14).
fn mesh_set_face_texture(req: &Request) -> String {
    geo_handler(req, op_mesh_set_face_texture)
}

/// `mesh.setMaterial`'s arm (`mcp_bridge.c:5976-5977`).
fn mesh_set_material(req: &Request) -> String {
    geo_handler(req, op_mesh_set_material)
}

// ---------------------------------------------------------------------------------------------
// mesh.get (bridge m3.12): the live read-back, a face/edge walk over Hammer's accessors
// ---------------------------------------------------------------------------------------------

/// One float, shortest round-trip form: `mesh.get` is read back into rebuilds, so the 3-decimal
/// `Sb::addf` the other replies use would drift a vertex by up to 0.0005 per round trip.
fn addf_exact(out: &mut Sb, v: f32) {
    if v.is_finite() {
        out.add(format!("{v}").as_bytes());
    } else {
        out.add(b"null");
    }
}

fn addf3_exact(out: &mut Sb, v: [f32; 3]) {
    out.add(b"[");
    addf_exact(out, v[0]);
    out.add(b",");
    addf_exact(out, v[1]);
    out.add(b",");
    addf_exact(out, v[2]);
    out.add(b"]");
}

/// `mesh.get`'s read gate: the active document (`-32003`), then the caller's `expectDoc` only
/// when one was given (a read needs no precondition, but a caller that names a document state
/// gets `-32004` when it moved). No editability test and nothing is opened or recorded.
pub(crate) fn read_gate(chain: &dyn Chain, params: Option<&str>) -> Result<Doc, OpErr> {
    let doc = active_doc_with(chain).map_err(|reason| OpErr::new(-32003, "NoActiveDocument", reason))?;
    let bytes = params.map(str::as_bytes);
    if bytes.is_some_and(|b| crate::entity_json::json_find_top(b, b"expectDoc").is_some()) {
        let (doc_seq, epoch) = fingerprint_ids();
        expect_doc_ok(bytes, doc_seq, epoch)
            .map_err(|why| OpErr::new(-32004, "DocumentChanged", why))?;
    }
    Ok(doc)
}

/// The most bytes one coordinate prints as (`f32` shortest round-trip text of any value within the
/// bridge's coordinate range, with sign, leading zeros and the point).
const REPLY_FLOAT_TEXT: usize = 24;
/// What the reply carries besides the lists: the keys, origin/angles/scales, parent chain, counts
/// and the `doc` tail (the envelope is outside the buffer, covered by its slack).
const REPLY_FIXED_BYTES: usize = 1024;
/// The result buffer of `mesh.get`: the bridge's maximum response line (`maxLineBytes`, which the
/// IPC reader on the client side enforces) less the envelope's slack.
const MESH_GET_BUFFER: usize = crate::control::MAX_LINE as usize - 1024;

/// The node's ancestors, nearest first, as classes (the walk ends at the document's world, which
/// is listed last), and whether the mesh's geometry is world-space: only `CMapWorld`,
/// `CMapGroup` and `CMapEntity` ancestors are proven to store their children in world space
/// (`tie_ancestors_ok`'s rule). An unreadable parent or a walk deeper than 64 is `false`, never a
/// guess.
fn parent_chain(host: &dyn Mesh, doc: Doc, mesh: Node) -> (Vec<&'static str>, bool) {
    let world = host.world(doc);
    let mut chain = Vec::new();
    let mut node = mesh;
    let mut proven = true;
    for _ in 0..64 {
        let Some(parent) = host.node_parent(node) else {
            return (chain, false);
        };
        let class = host.class(parent);
        chain.push(class);
        if Some(parent) == world {
            return (chain, proven);
        }
        if class != "group" && class != "entity" {
            proven = false;
        }
        node = parent;
    }
    (chain, false)
}

/// `mesh.get` (m3.12): the mesh's own (node-local) geometry as a vertex list plus face loops.
///
/// Vertices are `CollectHandles(verts)` order with their positions from ONE
/// `HFN_MESH_VERTEX_POSITIONS` call (identity matrix; the checked accessor, never a half-edge
/// endpoint read); faces are `CollectHandles(faces)` order, each loop the edge walk's corners
/// (`EDGE_VERTEX` of the first edge, then of every `EDGE_NEXT` until the first edge returns),
/// mapped to vertex-list indices by handle.
fn op_mesh_get(
    host: &dyn Mesh,
    doc: Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    let (mesh, _) = geo_target_mesh(host, doc, params)?;
    let internal = |why: &'static str| OpErr::new(-32603, "InternalError", why);
    let budget = out.capacity();
    let (vertex_slots, face_slots) = host.mesh_counts(mesh);

    // The node's own transform first: a failed read is an error, never a zero origin (a rebuild
    // from a wrong origin would move the mesh). Angles/scales are the C's unguarded field reads.
    let origin = host.origin(mesh).ok_or_else(|| internal("could not read the mesh origin"))?;
    let (parent_chain, world_space) = parent_chain(host, doc, mesh);

    // Refuse above the crate's limits, or above what one reply line can hold, with the counts
    // and BEFORE the walk (an estimate for the part not known yet: the corners).
    let too_large = |why: String, v: i64, f: i64| {
        OpErr::with_data(-32602, "InvalidParams", |db| {
            db.add(b"{\"reason\":");
            db.addjson(why.as_bytes());
            db.add(b",\"vertexCount\":");
            db.addi(v);
            db.add(b",\"faceCount\":");
            db.addi(f);
            db.add(b",\"maxLineBytes\":");
            db.addnum(u64::from(crate::control::MAX_LINE));
            db.add(b"}");
        })
    };
    let limits = |v: i64, f: i64| {
        too_large(
            format!(
                "mesh too large for mesh.get: {v} vertices, {f} faces \
                 (limits {GEO_MAX_VERTS}/{GEO_MAX_FACES}/{GEO_MAX_IDX})"
            ),
            v,
            f,
        )
    };
    let reply_too_large =
        |v: i64, f: i64| too_large("mesh too large for one reply".to_string(), v, f);
    if vertex_slots as i64 > GEO_MAX_VERTS as i64 * 4 || face_slots > GEO_FACE_CAP {
        return Err(limits(i64::from(vertex_slots), i64::from(face_slots)));
    }
    // Worst-case text sizes: a float prints at most REPLY_FLOAT_TEXT bytes for any coordinate the
    // bridge accepts; an index at most the digits of the largest vertex index; a face at least
    // three corners and a faceMaterials entry.
    let index_text = vertex_slots.max(1).to_string().len() + 1;
    let min_face_bytes = 3 * index_text + 2 + 3;
    let vertex_bytes = 3 * REPLY_FLOAT_TEXT + 4;
    if REPLY_FIXED_BYTES
        + vertex_slots.max(0) as usize * vertex_bytes
        + face_slots.max(0) as usize * min_face_bytes
        > budget
    {
        return Err(reply_too_large(i64::from(vertex_slots), i64::from(face_slots)));
    }
    let vertex_handles = if vertex_slots > 0 {
        host.collect_vertices(mesh, vertex_slots as usize)
            .ok_or_else(|| internal("could not collect vertex handles"))?
    } else {
        Vec::new()
    };
    let face_handles = if face_slots > 0 {
        host.collect_faces(mesh, face_slots as usize)
            .ok_or_else(|| internal("could not collect face handles"))?
    } else {
        Vec::new()
    };
    if vertex_handles.len() > GEO_MAX_VERTS || face_handles.len() > GEO_MAX_FACES {
        return Err(limits(vertex_handles.len() as i64, face_handles.len() as i64));
    }
    let vertex_of: std::collections::HashMap<u32, usize> = vertex_handles
        .iter()
        .enumerate()
        .map(|(i, h)| (handle_key(h), i))
        .collect();

    // One checked batch read for every vertex position; false = no position stream.
    let vertices = if vertex_handles.is_empty() {
        Vec::new()
    } else {
        host.vertex_positions(mesh, &vertex_handles)
            .ok_or_else(|| internal("mesh has no position stream"))?
    };
    debug_assert_eq!(vertices.len(), vertex_handles.len());

    let mut faces: Vec<Vec<usize>> = Vec::with_capacity(face_handles.len());
    let mut face_materials: Vec<i32> = Vec::with_capacity(face_handles.len());
    let mut materials: Vec<Vec<u8>> = Vec::new();
    // table index -> dense index, so MATERIAL_NAME runs once per distinct table entry.
    let mut table_to_dense: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
    let mut total_idx = 0usize;
    for face in &face_handles {
        let first = host.face_first_edge(mesh, face);
        if handle_invalid(&first) {
            return Err(internal("a face has no first edge"));
        }
        let mut loop_ix: Vec<usize> = Vec::new();
        let mut edge = first;
        loop {
            // The bounds: a polygon has no more corners than the mesh has vertices (a longer ring
            // does not close), and the whole mesh stays inside the index cap (a legitimate mesh
            // above it is the too-large refusal with counts, not an internal error).
            if loop_ix.len() >= vertex_handles.len() {
                return Err(internal("face walk overran the vertex count (edge ring does not close)"));
            }
            total_idx += 1;
            if total_idx > GEO_MAX_IDX {
                return Err(too_large(
                    format!(
                        "mesh too large for mesh.get: over {GEO_MAX_IDX} face corners \
                         ({} vertices, {} faces)",
                        vertex_handles.len(),
                        face_handles.len()
                    ),
                    vertex_handles.len() as i64,
                    face_handles.len() as i64,
                ));
            }
            let vh = host.edge_vertex(mesh, &edge);
            let Some(&vi) = vertex_of.get(&handle_key(&vh)) else {
                return Err(internal("an edge's vertex is not in the vertex list"));
            };
            loop_ix.push(vi);
            edge = host.edge_next(mesh, &edge);
            if handle_invalid(&edge) {
                return Err(internal("a face's edge ring ends at an invalid edge"));
            }
            if handle_key(&edge) == handle_key(&first) {
                break;
            }
        }
        faces.push(loop_ix);
        let table_ix = host.face_material_index(mesh, face);
        let dense = if table_ix < 0 {
            -1
        } else if let Some(&d) = table_to_dense.get(&table_ix) {
            d
        } else {
            let name = host.material_name(mesh, table_ix);
            let d = match materials.iter().position(|m| *m == name) {
                Some(d) => d,
                None => {
                    materials.push(name);
                    materials.len() - 1
                }
            } as i32;
            table_to_dense.insert(table_ix, d);
            d
        };
        face_materials.push(dense);
    }
    // The exact corner count is known now: the pessimistic size of the whole reply must fit.
    let material_bytes: usize = materials.iter().map(|m| 6 * m.len() + 3).sum();
    if REPLY_FIXED_BYTES
        + vertices.len() * vertex_bytes
        + total_idx * index_text
        + faces.len() * (index_text + 8)
        + material_bytes
        > budget
    {
        return Err(reply_too_large(vertices.len() as i64, faces.len() as i64));
    }
    out.add(b"{\"nodeID\":");
    out.addi(i64::from(host.id(mesh).unwrap_or(-1)));
    out.add(b",\"class\":\"mesh\",\"origin\":");
    addf3_exact(out, origin);
    out.add(b",\"angles\":");
    addf3_exact(out, host.read_angles(mesh));
    out.add(b",\"scales\":");
    addf3_exact(out, host.read_scales(mesh));
    out.add(b",\"space\":\"local\",\"worldSpace\":");
    out.add(if world_space { b"true" } else { b"false" });
    out.add(b",\"parentChain\":[");
    for (i, class) in parent_chain.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.addjson(class.as_bytes());
    }
    out.add(b"],\"vertices\":[");
    for (i, v) in vertices.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        addf3_exact(out, *v);
    }
    out.add(b"],\"faces\":[");
    for (i, f) in faces.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.add(b"[");
        for (k, &vi) in f.iter().enumerate() {
            if k > 0 {
                out.add(b",");
            }
            out.addnum(vi as u64);
        }
        out.add(b"]");
    }
    out.add(b"],\"faceMaterials\":[");
    for (i, m) in face_materials.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.addi(i64::from(*m));
    }
    out.add(b"],\"materials\":[");
    for (i, m) in materials.iter().enumerate() {
        if i > 0 {
            out.add(b",");
        }
        out.addjson(m);
    }
    out.add(b"],\"vertexCount\":");
    out.addnum(vertices.len() as u64);
    out.add(b",\"faceCount\":");
    out.addnum(faces.len() as u64);
    out.add(b"}");
    Ok(())
}

/// `mesh.get`'s dispatch: the read gate, the body, the `doc` tail and the overflow check. No
/// undo group is opened and no history is touched (the tail only reads the undo state).
pub(crate) fn mesh_get_dispatch(
    host: &dyn Mesh,
    id: &str,
    params: Option<&str>,
    out: &mut Sb,
) -> String {
    let outcome = read_gate(host, params)
        .and_then(|doc| op_mesh_get(host, doc, params, out).map(|()| (doc, false)));
    write_finish(id, outcome, out, "mesh too large for one reply")
}

/// `mesh.get`'s handler.
fn mesh_get(req: &Request) -> String {
    let mut buf = vec![0u8; MESH_GET_BUFFER];
    let mut out = Sb::new(&mut buf);
    mesh_get_dispatch(&host(), &req.id, req.params.as_deref(), &mut out)
}

/// The rows this group serves.
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("mesh.create", mesh_create),
    ("mesh.setGeometry", mesh_set_geometry),
    ("mesh.setMaterial", mesh_set_material),
    ("mesh.get", mesh_get),
    ("mesh.setFaceTexture", mesh_set_face_texture),
];

// ---------------------------------------------------------------------------------------------
// The production mesh calls (Windows tools build only)
// ---------------------------------------------------------------------------------------------

/// The C's `S2MeshHandle` and `S2HandleVec` (`s2sdk.h:64-83`), plus the polygon group the
/// C declares locally (`mcp_bridge.c:3126-3128`). Every offset is asserted against the generated
/// `CUtlVector_CEntityHandle` (`S2_SIZEOF_/S2_OFF_...` in `s2sdk.h:78-83`).
#[cfg(windows)]
mod live {
    use core::ffi::c_void;
    use core::ffi::CStr;
    use core::mem::offset_of;
    use core::ptr;
    use std::ffi::CString;

    use s2sdk_sys::generated::EXTERNAL_CONST_BUFFER_MARKER;
    use s2sdk_sys::generated::{
        CMapNode, CMapRootElement, CUtlVector_CEntityHandle, Vector,
    };

    use crate::generated::pins;
    use crate::ops::hammer::bind::{hfn, HammerFns};
    use crate::ops::hammer::{Doc, Node};

    use super::{delete_buffer, FaceTex, GameMesh, Mesh, MeshHandle, GEO_UV_MODE, INVALID_HANDLE};
    use crate::ops::ident::mem_ok;

    /// A `matrix3x4_t` (3 rows of 4 floats, 48 bytes). The generated SDK keeps this type as an
    /// opaque byte blob (gen.py has no type map for it, so every `matrix3x4_t` member comes out as
    /// `[u8; 48]`), which cannot be built as an identity; this is the one local spelling, with its
    /// size asserted below. Only an identity is ever built.
    #[repr(C)]
    struct Matrix3x4([[f32; 4]; 3]);

    impl Matrix3x4 {
        const IDENTITY: Self = Self([
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ]);
    }

    /// `S2HandleVec` (`s2sdk.h:76`): `CUtlVector<S2MeshHandle>` with caller-owned memory.
    #[repr(C)]
    struct HandleVec {
        size: i32,
        pad: i32,
        mem: *mut MeshHandle,
        alloc: i32,
        flags: i32,
    }

    /// `GPolyRec` (`mcp_bridge.c:3127`): 24 bytes.
    #[repr(C)]
    struct GPolyRec {
        n: i32,
        pad: i32,
        pts: *mut GVec,
        zero: u64,
    }

    /// `GPolyGroup` (`mcp_bridge.c:3128`): 16 bytes.
    #[repr(C)]
    struct GPolyGroup {
        count: i32,
        pad: i32,
        recs: *mut GPolyRec,
    }

    /// `GVec` (`mcp_bridge.c:3126`).
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct GVec {
        x: f32,
        y: f32,
        z: f32,
    }

    // The C's `_Static_assert`s on the same shapes (`s2sdk.h:65`, `:78-83`).
    const _: () = {
        assert!(core::mem::size_of::<MeshHandle>() == 16);
        assert!(
            core::mem::size_of::<HandleVec>()
                == core::mem::size_of::<CUtlVector_CEntityHandle>()
        );
        assert!(offset_of!(HandleVec, size) == offset_of!(CUtlVector_CEntityHandle, m_Size));
        assert!(offset_of!(HandleVec, pad) == 4);
        assert!(offset_of!(HandleVec, mem) == offset_of!(CUtlVector_CEntityHandle, m_pMemory));
        assert!(
            offset_of!(HandleVec, alloc)
                == offset_of!(CUtlVector_CEntityHandle, m_nAllocationCount)
        );
        assert!(offset_of!(HandleVec, flags) == offset_of!(CUtlVector_CEntityHandle, m_nGrowSize));
        assert!(core::mem::size_of::<GPolyRec>() == 24);
        assert!(core::mem::size_of::<GPolyGroup>() == 16);
        assert!(core::mem::size_of::<GVec>() == 12);
        assert!(core::mem::size_of::<Matrix3x4>() == 48);
        assert!(core::mem::size_of::<Vector>() == 12);
    };

    /// `S2_UTLVEC_EXTERNAL`: the C's alias of the generated "caller-owned, never realloc"
    /// marker (`1 << 30`).
    const UTLVEC_EXTERNAL: i32 = EXTERNAL_CONST_BUFFER_MARKER as i32;

    /// The bound table: any body runs after `write_gate`, whose `resolve_hammer` bound it.
    fn table() -> HammerFns {
        hfn().expect("cs2-bridge: the hammer function table is bound before any mesh write")
    }

    /// One generated vtable slot the module gate verified (`PIN_SLOTS`); a null slot can only
    /// mean the generated table and the pin table disagree, and the C would call null.
    fn slot<T>(slot: Option<T>, what: &'static str) -> T {
        match slot {
            Some(slot) => slot,
            None => panic!("cs2-bridge: the verified vtable slot {what} is null"),
        }
    }

    /// A NUL-terminated copy of a name/material the C hands to Hammer as `const char *`.
    fn cstring(bytes: &[u8]) -> CString {
        CString::new(bytes)
            .expect("cs2-bridge: a mesh name or material carries no interior NUL")
    }

    /// `CMapMesh_PolyMesh` (`s2sdk.h:513`): the embedded `CHammerPolyMesh`.
    fn poly(mesh: Node) -> *mut u8 {
        (mesh.0 as *mut u8).wrapping_add(pins::HOFF_MESH_POLYMESH.value as usize)
    }

    /// `CMapNode_Undoable` (`s2sdk.h:498`): the `IUndoable` subobject.
    fn undoable(node: Node) -> *mut c_void {
        // SAFETY: the base offset is generated from the same IR as the C's `S2_BASEOFF`;
        // `node` passed the identity chain.
        unsafe {
            (node.0 as *mut u8)
                .add(CMapNode::BASE_OFFSET_IUndoable)
                .cast::<c_void>()
        }
    }

    impl Mesh for GameMesh {
        fn root_file_id(&self, doc: Doc) -> Option<u32> {
            let root = self.0.root(doc.0) as *mut CMapRootElement;
            if root.is_null() {
                return None; // `!root` (`:5397`)
            }
            // SAFETY: `root` is the gated document's `m_pRoot`; `addr_of!` computes the field
            // address without forming a reference. The C reads the same field after its
            // `mem_ok` probe (`:5397`).
            let field = unsafe { ptr::addr_of!((*root).base.m_fileId) };
            if !mem_ok(field.cast()) {
                return None;
            }
            // SAFETY: the field's first byte was just proven readable; `DmFileId_t` is a
            // `u32` wrapper (`generated/datamodel.rs:83`) and the C passes it as a `u32`.
            Some(unsafe { field.read_unaligned() }.0)
        }

        fn is_mesh(&self, node: Node) -> bool {
            // `obj_is(n, PIN_HAMMER, HV_MAPMESH_VT)` (`:3326`): the same identity check
            // `node_class` makes first (`:3298-3299`).
            self.0.class(node) == "mesh"
        }

        fn create_mesh_element(
            &self,
            file_id: u32,
            name: u64,
            src: &'static CStr,
            line: i32,
        ) -> Option<Node> {
            let table = table();
            // SAFETY: `HFN_CREATE_MESH_ELEMENT` is the verified hammer.dll function
            // (`s2sdk.h:728`); `src` is the C's `"mcp_bridge"` literal (static storage
            // Hammer may keep) and the other arguments are the C's own literals (`:5399`).
            let mesh = unsafe { (table.create_mesh_element)(file_id, name, src.as_ptr(), line) };
            (!mesh.is_null()).then_some(Node(mesh as usize))
        }

        fn insert_node(&self, doc: Doc, node: Node) {
            let table = table();
            // SAFETY: verified HFN_INSERT_NODE; the C passes the gated document, the created
            // mesh node and a null parent (`:5403`).
            unsafe {
                (table.insert_node)(
                    doc.0 as *mut c_void,
                    node.0 as *mut c_void,
                    ptr::null_mut(),
                )
            };
        }

        fn record_node(&self, hist: usize, node: Node) {
            let table = table();
            // SAFETY: `hist` is `undo_open`'s token and `undoable` the node's IUndoable
            // subobject; the C's recursion flag is 1 (`:5404`).
            unsafe { (table.record_node)(hist as *mut c_void, undoable(node), 1) };
        }

        fn mesh_counts(&self, node: Node) -> (i32, i32) {
            let poly = poly(node);
            // SAFETY: `node` passed `is_mesh` (the pinned CMapMesh vtable); the two 4-byte
            // counts are the C's unguarded `hm->vertices.count` / `hm->faces.count`
            // (`s2sdk.h:480-489`, `mcp_bridge.c:4742-4743`).
            unsafe {
                (
                    poly.wrapping_add(pins::HOFF_POLYMESH_VERTS.value as usize)
                        .cast::<i32>()
                        .read_unaligned(),
                    poly.wrapping_add(pins::HOFF_POLYMESH_FACES.value as usize)
                        .cast::<i32>()
                        .read_unaligned(),
                )
            }
        }

        fn collect_faces(&self, node: Node, cap: usize) -> Option<Vec<MeshHandle>> {
            let table = table();
            // The C's `geo_alloc(sizeof(S2MeshHandle) * n)`: zeroed, caller-owned.
            let mut handles = vec![[0u8; 16]; cap];
            let mut out = HandleVec {
                size: 0,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: cap as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_COLLECT_HANDLES; `list` is the identity-checked mesh's faces
            // vector and `out` a caller-owned buffer of `cap` handles (`:4678-4680`).
            let ok = unsafe {
                (table.collect_handles)(
                    poly(node).wrapping_add(pins::HOFF_POLYMESH_FACES.value as usize).cast(),
                    (&mut out as *mut HandleVec).cast(),
                )
            };
            if ok == 0 {
                return None;
            }
            // The C trusts `out.size`; clamping to the buffer is a memory-safety fix that cannot
            // change a reply for a game that fills at most `alloc` entries.
            handles.truncate(out.size.clamp(0, cap as i32) as usize);
            Some(handles)
        }

        fn init_from_polygons(
            &self,
            node: Node,
            polys: &[Vec<[f32; 3]>],
            defmat: &[u8],
            cap: usize,
        ) -> Vec<MeshHandle> {
            let table = table();
            // :4706-4714: one rec per face over a flat point array, exactly as the C fills it.
            let total: usize = polys.iter().map(Vec::len).sum();
            let mut pts: Vec<GVec> = vec![
                GVec {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0
                };
                total
            ];
            let mut recs: Vec<GPolyRec> = Vec::with_capacity(polys.len());
            let mut offset = 0usize;
            for poly in polys {
                recs.push(GPolyRec {
                    n: poly.len() as i32,
                    pad: 0,
                    // SAFETY: `pts` has a fixed length and is never reallocated before the call.
                    pts: unsafe { pts.as_mut_ptr().add(offset) },
                    zero: 0,
                });
                for (k, point) in poly.iter().enumerate() {
                    pts[offset + k] = GVec {
                        x: point[0],
                        y: point[1],
                        z: point[2],
                    };
                }
                offset += poly.len();
            }
            let mut group = GPolyGroup {
                count: recs.len() as i32,
                pad: 0,
                recs: recs.as_mut_ptr(),
            };
            let mut handles = vec![[0u8; 16]; cap];
            let mut out = HandleVec {
                size: 0,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: cap as i32,
                flags: UTLVEC_EXTERNAL,
            };
            let material = cstring(defmat);
            // SAFETY: verified HFN_MESH_INIT_FROM_POLYGONS (`s2sdk.h:729`): the mesh
            // passed the identity check, `group`/`out` are the C's local structs (live for the
            // call), `material` is NUL-terminated and `uvMode` is the C's `GEO_UV_MODE`.
            unsafe {
                (table.mesh_init_from_polygons)(
                    node.0 as *mut c_void,
                    (&mut group as *mut GPolyGroup).cast(),
                    material.as_ptr(),
                    GEO_UV_MODE,
                    (&mut out as *mut HandleVec).cast(),
                )
            };
            handles.truncate(out.size.clamp(0, cap as i32) as usize);
            handles
        }

        fn delete_faces(&self, node: Node, faces: &[MeshHandle], alloc: usize) {
            let table = table();
            // :4677-4681: the C's buffer is `geo_alloc(sizeof(S2MeshHandle) * n)` with
            // `v.alloc = n`; `CollectHandles` fills `v.size <= n` of it and `MeshDeleteFaces`
            // takes the whole vector. Passing a buffer sized to the collected count would let
            // Hammer write up to `alloc` handles past it, so the buffer is `alloc` handles with
            // the collected ones copied to the front.
            let (mut handles, size) = delete_buffer(faces, alloc);
            let mut out = HandleVec {
                size,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: alloc as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_MESH_DELETE_FACES (`s2sdk.h:732`) over the vector
            // `CollectHandles` just filled; `handles` holds `alloc` handles and outlives the
            // call (`:4681`).
            unsafe {
                (table.mesh_delete_faces)(
                    node.0 as *mut c_void,
                    (&mut out as *mut HandleVec).cast(),
                )
            };
        }

        fn assign_material(&self, node: Node, faces: &[MeshHandle], material: &[u8]) {
            let table = table();
            let handles = faces;
            let material = cstring(material);
            // SAFETY: verified HFN_ASSIGN_MATERIAL_TO_FACES (`s2sdk.h:730`), called with
            // the mesh's `CHammerPolyMesh` and a handle slice that outlives the call (`:4726`,
            // `:5465`).
            unsafe {
                (table.assign_material_to_faces)(
                    poly(node).cast(),
                    handles.as_ptr().cast::<c_void>(),
                    handles.len() as i32,
                    material.as_ptr(),
                )
            };
        }

        fn read_scales(&self, node: Node) -> [f32; 3] {
            let node = node.0 as *mut CMapNode;
            // SAFETY: `node` passed `is_mesh`; the C reads the field unguarded (`:4691`).
            unsafe { [(*node).base.m_Scales.x, (*node).base.m_Scales.y, (*node).base.m_Scales.z] }
        }

        fn read_angles(&self, node: Node) -> [f32; 3] {
            let node = node.0 as *mut CMapNode;
            // SAFETY: `node` passed `is_mesh`; the C reads the field unguarded (`:4695`).
            unsafe { [(*node).base.m_Angles.x, (*node).base.m_Angles.y, (*node).base.m_Angles.z] }
        }

        fn node_parent(&self, node: Node) -> Option<Node> {
            let node = node.0 as *mut CMapNode;
            // SAFETY: `addr_of!` computes the member's address only; the caller passed a node a
            // walk reached (`geo_target_mesh`) or one of its ancestors read here.
            let field = unsafe { ptr::addr_of!((*node).m_pParent) };
            if !mem_ok(field.cast()) {
                return None;
            }
            // SAFETY: the pointer member was just proven readable.
            let parent = unsafe { field.read_unaligned() };
            if parent.is_null() || !mem_ok(parent.cast()) {
                return None;
            }
            Some(Node(parent as usize))
        }

        fn set_pivot(&self, node: Node, value: [f32; 3]) {
            let node = node.0 as *mut CMapNode;
            let pos = Vector {
                x: value[0],
                y: value[1],
                z: value[2],
            };
            // SAFETY: slot 140 of the mesh's verified vtable is `CMapNode::SetPivotPosition`
            // (`s2sdk.h:475`, `:4700`).
            unsafe {
                slot(
                    CMapNode::vtable(node).SetPivotPosition,
                    "CMapNode::SetPivotPosition",
                )(node.cast(), &pos)
            };
        }

        fn collect_vertices(&self, node: Node, cap: usize) -> Option<Vec<MeshHandle>> {
            let table = table();
            let mut handles = vec![[0u8; 16]; cap];
            let mut out = HandleVec {
                size: 0,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: cap as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_COLLECT_HANDLES over the identity-checked mesh's vertices
            // vector, into a caller-owned buffer of `cap` handles (as `collect_faces`).
            let ok = unsafe {
                (table.collect_handles)(
                    poly(node).wrapping_add(pins::HOFF_POLYMESH_VERTS.value as usize).cast(),
                    (&mut out as *mut HandleVec).cast(),
                )
            };
            if ok == 0 {
                return None;
            }
            handles.truncate(out.size.clamp(0, cap as i32) as usize);
            Some(handles)
        }

        fn face_first_edge(&self, node: Node, face: &MeshHandle) -> MeshHandle {
            let table = table();
            let mut out = INVALID_HANDLE;
            // SAFETY: verified HFN_MESH_FACE_FIRST_EDGE (re-report R1): plain `__fastcall`
            // `(poly, out*, face*)`; `out` and `face` are 16-byte handles that outlive the call.
            unsafe {
                (table.mesh_face_first_edge)(
                    poly(node).cast(),
                    out.as_mut_ptr().cast(),
                    face.as_ptr().cast(),
                )
            };
            out
        }

        fn edge_next(&self, node: Node, edge: &MeshHandle) -> MeshHandle {
            let table = table();
            let mut out = INVALID_HANDLE;
            // SAFETY: verified HFN_MESH_EDGE_NEXT, same shape as `face_first_edge`.
            unsafe {
                (table.mesh_edge_next)(
                    poly(node).cast(),
                    out.as_mut_ptr().cast(),
                    edge.as_ptr().cast(),
                )
            };
            out
        }

        fn edge_vertex(&self, node: Node, edge: &MeshHandle) -> MeshHandle {
            let table = table();
            let mut out = INVALID_HANDLE;
            // SAFETY: verified HFN_MESH_EDGE_VERTEX, same shape as `face_first_edge`.
            unsafe {
                (table.mesh_edge_vertex)(
                    poly(node).cast(),
                    out.as_mut_ptr().cast(),
                    edge.as_ptr().cast(),
                )
            };
            out
        }

        fn vertex_positions(&self, node: Node, verts: &[MeshHandle]) -> Option<Vec<[f32; 3]>> {
            if verts.is_empty() {
                return Some(Vec::new());
            }
            let table = table();
            let identity = Matrix3x4::IDENTITY;
            // Pre-zeroed and sized n (a failing vertex keeps (0,0,0), as Hammer writes it).
            let mut out = vec![Vector { x: 0.0, y: 0.0, z: 0.0 }; verts.len()];
            // SAFETY: verified HFN_MESH_VERTEX_POSITIONS (re-report FOLLOW-UP):
            // `bool (poly, const MeshHandle *verts /*n x 16*/, int n, const matrix3x4_t *xf,
            // Vector *out /*n x 12*/)`; `verts`, the 48-byte identity and `out` (n vectors) all
            // outlive the call and are never aliased by Hammer.
            let ok = unsafe {
                (table.mesh_vertex_positions)(
                    poly(node).cast(),
                    verts.as_ptr().cast(),
                    verts.len() as i32,
                    (&identity as *const Matrix3x4).cast(),
                    out.as_mut_ptr().cast(),
                )
            };
            (ok != 0).then(|| out.iter().map(|v| [v.x, v.y, v.z]).collect())
        }

        fn face_material_index(&self, node: Node, face: &MeshHandle) -> i32 {
            let table = table();
            // SAFETY: verified HFN_MESH_FACE_MATERIAL_INDEX: `(poly, const face*) -> int`.
            unsafe { (table.mesh_face_material_index)(poly(node).cast(), face.as_ptr().cast()) }
        }

        fn material_name(&self, node: Node, index: i32) -> Vec<u8> {
            let table = table();
            // SAFETY: verified HFN_MESH_MATERIAL_NAME. The caller passes only an index
            // `face_material_index` returned (>= 0; the function's bounds check never fires).
            // The pointer is mesh-owned and valid until the mesh changes: copied right here.
            let p = unsafe { (table.mesh_material_name)(poly(node).cast(), index) };
            if p.is_null() {
                return Vec::new();
            }
            // SAFETY: a non-null result is a NUL-terminated string owned by the mesh.
            unsafe { CStr::from_ptr(p) }.to_bytes().to_vec()
        }

        fn face_texture(&self, node: Node, face: &MeshHandle, set: u32) -> FaceTex {
            let table = table();
            let mut tex = FaceTex::unread();
            // SAFETY: verified HFN_MESH_GET_FACE_TEXTURE: `(poly, const handle*, set, Vec4*,
            // Vec4*, Vec2*)`, a pure read into the three caller buffers (4, 4 and 2 floats).
            unsafe {
                (table.mesh_get_face_texture)(
                    poly(node).cast(),
                    face.as_ptr().cast(),
                    set,
                    tex.u.as_mut_ptr(),
                    tex.v.as_mut_ptr(),
                    tex.scale.as_mut_ptr(),
                )
            };
            tex
        }

        fn set_face_texture(&self, node: Node, face: &MeshHandle, set: u32, tex: &FaceTex) {
            let table = table();
            let mut handles = [*face];
            let mut vec = HandleVec {
                size: 1,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: 1,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_MESH_SET_FACE_TEXTURE: `(poly, HandleVec*, set, const Vec4*,
            // const Vec4*, const Vec2*)`; the vector holds one live handle and outlives the call.
            unsafe {
                (table.mesh_set_face_texture)(
                    poly(node).cast(),
                    (&mut vec as *mut HandleVec).cast(),
                    set,
                    tex.u.as_ptr(),
                    tex.v.as_ptr(),
                    tex.scale.as_ptr(),
                )
            };
        }

        fn align_face_textures(
            &self,
            node: Node,
            faces: &[MeshHandle],
            set: u32,
            mode: i32,
            flags: i32,
        ) {
            let table = table();
            let mut handles = faces.to_vec();
            let mut vec = HandleVec {
                size: handles.len() as i32,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: handles.len() as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_MESH_ALIGN_FACE_TEXTURE: `(CMapMesh*, HandleVec*, set, mode,
            // flags, const float origin[12])`; the node passed `is_mesh`, `handles` outlives the
            // call and IDENTITY_3X4 is the 12 floats the function copies.
            unsafe {
                (table.mesh_align_face_texture)(
                    node.0 as *mut c_void,
                    (&mut vec as *mut HandleVec).cast(),
                    set,
                    mode,
                    flags,
                    super::IDENTITY_3X4.as_ptr(),
                )
            };
        }

        fn justify_face_textures(&self, node: Node, faces: &[MeshHandle], set: u32, mode: u32) {
            let table = table();
            let mut handles = faces.to_vec();
            let mut vec = HandleVec {
                size: handles.len() as i32,
                pad: 0,
                mem: handles.as_mut_ptr(),
                alloc: handles.len() as i32,
                flags: UTLVEC_EXTERNAL,
            };
            // SAFETY: verified HFN_MESH_JUSTIFY_FACE_TEXTURE: `(CMapMesh*, HandleVec*, set, mode,
            // bounds = NULL)`: with no bounds each face is justified to its own extent.
            unsafe {
                (table.mesh_justify_face_texture)(
                    node.0 as *mut c_void,
                    (&mut vec as *mut HandleVec).cast(),
                    set,
                    mode,
                    ptr::null(),
                )
            };
        }
    }
}

/// Non-Windows: no Hammer module exists, so the `-32017` gate answers before any mesh body
/// runs (the assembly's `module_reason` reports `hammer_not_loaded`). Anything that still
/// reaches a mesh call fails loudly instead of pretending it wrote.
#[cfg(not(windows))]
impl Mesh for GameMesh {
    fn root_file_id(&self, _doc: Doc) -> Option<u32> {
        no_mesh("mesh.create's document root")
    }
    fn is_mesh(&self, _node: Node) -> bool {
        no_mesh("a mesh's identity")
    }
    fn create_mesh_element(
        &self,
        _file_id: u32,
        _name: u64,
        _src: &'static CStr,
        _line: i32,
    ) -> Option<Node> {
        no_mesh("mesh.create's element")
    }
    fn insert_node(&self, _doc: Doc, _node: Node) {
        no_mesh("mesh.create's insert")
    }
    fn record_node(&self, _hist: usize, _node: Node) {
        no_mesh("mesh.create's undo record")
    }
    fn mesh_counts(&self, _node: Node) -> (i32, i32) {
        no_mesh("a mesh's component counts")
    }
    fn collect_faces(&self, _node: Node, _cap: usize) -> Option<Vec<MeshHandle>> {
        no_mesh("a mesh's face handles")
    }
    fn init_from_polygons(
        &self,
        _node: Node,
        _polys: &[Vec<[f32; 3]>],
        _defmat: &[u8],
        _cap: usize,
    ) -> Vec<MeshHandle> {
        no_mesh("mesh.create's polygons")
    }
    fn delete_faces(&self, _node: Node, _faces: &[MeshHandle], _alloc: usize) {
        no_mesh("a mesh's face delete")
    }
    fn assign_material(&self, _node: Node, _faces: &[MeshHandle], _material: &[u8]) {
        no_mesh("a mesh's material")
    }
    fn read_scales(&self, _node: Node) -> [f32; 3] {
        no_mesh("a mesh's scales")
    }
    fn read_angles(&self, _node: Node) -> [f32; 3] {
        no_mesh("a mesh's angles")
    }
    fn node_parent(&self, _node: Node) -> Option<Node> {
        no_mesh("a mesh's parent")
    }
    fn set_pivot(&self, _node: Node, _value: [f32; 3]) {
        no_mesh("a mesh's pivot write")
    }
    fn collect_vertices(&self, _node: Node, _cap: usize) -> Option<Vec<MeshHandle>> {
        no_mesh("a mesh's vertex handles")
    }
    fn face_first_edge(&self, _node: Node, _face: &MeshHandle) -> MeshHandle {
        no_mesh("a face's first edge")
    }
    fn edge_next(&self, _node: Node, _edge: &MeshHandle) -> MeshHandle {
        no_mesh("an edge's successor")
    }
    fn edge_vertex(&self, _node: Node, _edge: &MeshHandle) -> MeshHandle {
        no_mesh("an edge's vertex")
    }
    fn vertex_positions(&self, _node: Node, _verts: &[MeshHandle]) -> Option<Vec<[f32; 3]>> {
        no_mesh("vertex positions")
    }
    fn face_material_index(&self, _node: Node, _face: &MeshHandle) -> i32 {
        no_mesh("a face's material index")
    }
    fn material_name(&self, _node: Node, _index: i32) -> Vec<u8> {
        no_mesh("a material name")
    }
    fn face_texture(&self, _node: Node, _face: &MeshHandle, _set: u32) -> FaceTex {
        no_mesh("a face's texture read")
    }
    fn set_face_texture(&self, _node: Node, _face: &MeshHandle, _set: u32, _tex: &FaceTex) {
        no_mesh("a face's texture write")
    }
    fn align_face_textures(&self, _node: Node, _faces: &[MeshHandle], _set: u32, _mode: i32, _flags: i32) {
        no_mesh("a face texture align")
    }
    fn justify_face_textures(&self, _node: Node, _faces: &[MeshHandle], _set: u32, _mode: u32) {
        no_mesh("a face texture justify")
    }
}

/// The non-Windows refusal: a mesh body must never run there (the gate answers first).
#[cfg(not(windows))]
fn no_mesh(what: &str) -> ! {
    panic!("cs2-bridge: {what} needs the Windows tools build (Hammer is a Windows module)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // Fabricated pointers: the fake only needs distinct tokens, never dereferences.
    const WORLD: Node = Node(0x8000);
    const NODE: Node = Node(0x9000);

    /// The recording host: every [`Mesh`] call appends its arguments, the `Chain`/`Nodes` hops
    /// answer the editable-document chain, and the mesh fields are the test's.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        /// `face_texture`/`set_face_texture`'s store, one entry per `box_topo` face.
        tex: RefCell<Vec<FaceTex>>,
        /// Faces whose texture read answers the NaN pre-fill (an absent set / stale face).
        tex_nan: RefCell<Vec<usize>>,
        /// `align`/`justify` leaves face 0 unreadable (the post-write failure).
        nan_after_write: bool,
        /// `set_face_texture` of this face makes it unreadable afterwards (NaN mid-loop).
        nan_after_set: Option<usize>,
        /// `set_face_texture` stores nothing (the write did not land).
        drop_set: bool,
        /// `align` leaves a non-zero offset behind (flag 4 not honoured).
        align_offset: bool,
        /// `collect_faces` answers this many handles (past the 1000-row reply cap).
        tex_big: Option<usize>,
        /// Every `assign_material` call's handles and material (`geo_build`'s mapping, `:4719`).
        assigns: RefCell<Vec<(Vec<MeshHandle>, Vec<u8>)>>,
        file_id: Option<u32>,
        mesh_class: bool,
        vertex_slots: i32,
        face_slots: i32,
        node_id: Option<i32>,
        origin: Option<[f32; 3]>,
        scales: [f32; 3],
        angles: [f32; 3],
        collect_fails: bool,
        collect_size: Option<usize>,
        init_size: Option<usize>,
        /// mesh.get's model of Hammer's half-edge mesh.
        topo: Option<Topo>,
        /// `edge_next` never closes the ring (the walk-overrun guard's input).
        overrun: bool,
        /// `face_first_edge` answers the invalid handle.
        bad_first_edge: bool,
        /// `collect_vertices` leaves this vertex out of the list.
        hide_vertex: Option<usize>,
        /// `vertex_positions` answers false (no position stream).
        no_positions: bool,
        /// The classes of the mesh's ancestors strictly below the world, nearest first.
        chain: Vec<&'static str>,
        /// `node_parent` answers `None` (an unreadable parent field).
        parent_broken: bool,
    }

    /// The Fake's half-edge mesh: `verts`, each face a loop of vertex indices, the face's
    /// material table index and the table's names. Handle keys: vertex `0x500+i`, face
    /// `0x100+f`, the edge of corner `k` of face `f` `0x1000 + f*2^21 + k` (it points TO
    /// corner `k`'s vertex).
    #[derive(Clone, Default)]
    struct Topo {
        verts: Vec<[f32; 3]>,
        faces: Vec<Vec<usize>>,
        face_mat: Vec<i32>,
        names: Vec<&'static str>,
    }

    fn key_handle(key: u32) -> MeshHandle {
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&key.to_le_bytes());
        h
    }

    fn unkey(h: &MeshHandle) -> u32 {
        handle_key(h)
    }

    impl Topo {
        fn edge_corner(edge: &MeshHandle) -> (usize, usize) {
            let k = unkey(edge) - 0x1000;
            ((k >> 21) as usize, (k & 0x1F_FFFF) as usize)
        }
    }

    const APP: usize = 0x1000;
    const SESSION: usize = 0x2000;
    const DOCSESSION: usize = 0x3000;
    const HISTORY: usize = 0x7000;

    impl Fake {
        fn mesh() -> Self {
            Self {
                file_id: Some(7),
                mesh_class: true,
                vertex_slots: 8,
                face_slots: 6,
                node_id: Some(42),
                origin: Some([1.0, 2.5, -3.0]),
                scales: [1.0, 1.0, 1.0],
                ..Self::default()
            }
        }

        fn log(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }

        fn take(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Chain for Fake {
        fn module_reason(&self) -> Option<&'static str> {
            None
        }
        fn app(&self) -> Option<usize> {
            Some(APP)
        }
        fn session(&self, _app: usize) -> Option<usize> {
            Some(SESSION)
        }
        fn doc_session(&self, _session: usize) -> Option<usize> {
            Some(DOCSESSION)
        }
        fn owner(&self, _doc_session: usize) -> usize {
            SESSION
        }
        fn doc_of(&self, _doc_session: usize) -> usize {
            Doc(0x4000).0
        }
        fn doc_backptr(&self, _doc: usize) -> usize {
            DOCSESSION
        }
        fn is_of(&self, _object: usize, _vtable: u32) -> bool {
            true
        }
        fn root(&self, _doc: usize) -> usize {
            0x5000
        }
        fn active_flag(&self, _doc: usize) -> bool {
            true
        }
        fn selection(&self, _doc: usize) -> usize {
            0x6000
        }
        fn scope_depth(&self, _doc: usize) -> u32 {
            0
        }
        fn undo_history(&self, _doc: usize) -> usize {
            HISTORY
        }
        fn active_history(&self) -> usize {
            HISTORY
        }
    }

    impl Nodes for Fake {
        fn children(&self, node: Node) -> Option<Vec<Node>> {
            if node == WORLD {
                Some(vec![NODE])
            } else {
                Some(Vec::new())
            }
        }
        fn class(&self, node: Node) -> &'static str {
            if node == WORLD {
                "world"
            } else if node.0 >= 0xC000 {
                self.chain[node.0 - 0xC000]
            } else if self.mesh_class {
                "mesh"
            } else {
                "group"
            }
        }
        fn id(&self, _node: Node) -> Option<i32> {
            self.node_id
        }
        fn origin(&self, _node: Node) -> Option<[f32; 3]> {
            self.origin
        }
        fn mesh_slots(&self, _node: Node) -> Option<(i32, i32)> {
            Some((self.vertex_slots, self.face_slots))
        }
        fn world(&self, _doc: Doc) -> Option<Node> {
            Some(WORLD)
        }
    }

    impl Mesh for Fake {
        fn root_file_id(&self, _doc: Doc) -> Option<u32> {
            self.log("root_file_id".to_string());
            self.file_id
        }
        fn is_mesh(&self, node: Node) -> bool {
            self.log(format!("is_mesh {:#x}", node.0));
            self.mesh_class
        }
        fn create_mesh_element(
            &self,
            file_id: u32,
            name: u64,
            src: &'static CStr,
            line: i32,
        ) -> Option<Node> {
            self.log(format!(
                "create file={file_id} name={name} src={} line={line}",
                String::from_utf8_lossy(src.to_bytes())
            ));
            Some(Node(0xA000))
        }
        fn insert_node(&self, _doc: Doc, node: Node) {
            self.log(format!("insert {:#x}", node.0));
        }
        fn record_node(&self, hist: usize, node: Node) {
            self.log(format!("record_node {hist:#x} {:#x}", node.0));
        }
        fn mesh_counts(&self, _node: Node) -> (i32, i32) {
            (self.vertex_slots, self.face_slots)
        }
        fn collect_faces(&self, _node: Node, cap: usize) -> Option<Vec<MeshHandle>> {
            self.log(format!("collect cap={cap}"));
            if self.collect_fails {
                return None;
            }
            if let Some(n) = self.tex_big {
                return Some((0..n).map(|f| key_handle(0x100 + f as u32)).collect());
            }
            if let Some(t) = &self.topo {
                return Some((0..t.faces.len()).map(|f| key_handle(0x100 + f as u32)).collect());
            }
            let n = self.collect_size.unwrap_or(cap);
            Some(vec![[0x11u8; 16]; n])
        }
        fn init_from_polygons(
            &self,
            _node: Node,
            polys: &[Vec<[f32; 3]>],
            defmat: &[u8],
            cap: usize,
        ) -> Vec<MeshHandle> {
            self.log(format!(
                "init nf={} npts={} mat={} cap={cap}",
                polys.len(),
                polys.iter().map(Vec::len).sum::<usize>(),
                String::from_utf8_lossy(defmat)
            ));
            let n = self.init_size.unwrap_or(cap);
            // Distinct handles, so a test can tell which created face a material went to.
            (0..n).map(|face| [0xA0 + face as u8; 16]).collect()
        }
        fn delete_faces(&self, _node: Node, faces: &[MeshHandle], alloc: usize) {
            // The same buffer shape the live impl hands Hammer: `alloc` handles, the collected
            // ones at the front (`mcp_bridge.c:4677-4681`).
            let (buffer, size) = delete_buffer(faces, alloc);
            self.log(format!(
                "delete size={size} alloc={alloc} buf={}",
                buffer.len()
            ));
        }
        fn assign_material(&self, _node: Node, faces: &[MeshHandle], material: &[u8]) {
            self.log(format!(
                "assign n={} mat={}",
                faces.len(),
                String::from_utf8_lossy(material)
            ));
            self.assigns
                .borrow_mut()
                .push((faces.to_vec(), material.to_vec()));
        }
        fn read_scales(&self, _node: Node) -> [f32; 3] {
            self.scales
        }
        fn read_angles(&self, _node: Node) -> [f32; 3] {
            self.angles
        }
        fn node_parent(&self, node: Node) -> Option<Node> {
            if self.parent_broken {
                return None;
            }
            let next = if node.0 >= 0xC000 { node.0 - 0xC000 + 1 } else { 0 };
            Some(if next < self.chain.len() { Node(0xC000 + next) } else { WORLD })
        }
        fn set_pivot(&self, _node: Node, value: [f32; 3]) {
            self.log(format!("set_pivot {value:?}"));
        }
        fn collect_vertices(&self, _node: Node, _cap: usize) -> Option<Vec<MeshHandle>> {
            self.log("collect_vertices".to_string());
            let t = self.topo.as_ref()?;
            Some(
                (0..t.verts.len())
                    .filter(|i| self.hide_vertex != Some(*i))
                    .map(|i| key_handle(0x500 + i as u32))
                    .collect(),
            )
        }
        fn face_first_edge(&self, _node: Node, face: &MeshHandle) -> MeshHandle {
            let f = unkey(face).wrapping_sub(0x100);
            if !self.bad_first_edge && self.topo.as_ref().is_some_and(|t| (f as usize) < t.faces.len()) {
                key_handle(0x1000 + (f << 21))
            } else {
                INVALID_HANDLE
            }
        }
        fn edge_next(&self, _node: Node, edge: &MeshHandle) -> MeshHandle {
            let t = self.topo.as_ref().expect("topology");
            let (f, k) = Topo::edge_corner(edge);
            let next = if self.overrun { k + 1 } else { (k + 1) % t.faces[f].len() };
            key_handle(0x1000 + ((f as u32) << 21) + next as u32)
        }
        fn edge_vertex(&self, _node: Node, edge: &MeshHandle) -> MeshHandle {
            let t = self.topo.as_ref().expect("topology");
            let (f, k) = Topo::edge_corner(edge);
            key_handle(0x500 + t.faces[f][k % t.faces[f].len()] as u32)
        }
        fn vertex_positions(&self, _node: Node, verts: &[MeshHandle]) -> Option<Vec<[f32; 3]>> {
            self.log(format!("vertex_positions n={}", verts.len()));
            if self.no_positions {
                return None;
            }
            let t = self.topo.as_ref().expect("topology");
            Some(verts.iter().map(|h| t.verts[(unkey(h) - 0x500) as usize]).collect())
        }
        fn face_material_index(&self, _node: Node, face: &MeshHandle) -> i32 {
            let t = self.topo.as_ref().expect("topology");
            t.face_mat[(unkey(face) - 0x100) as usize]
        }
        fn face_texture(&self, _node: Node, face: &MeshHandle, set: u32) -> FaceTex {
            let f = (unkey(face) - 0x100) as usize;
            self.log(format!("get_tex f={f} set={set}"));
            if self.tex_nan.borrow().contains(&f) {
                return FaceTex::unread();
            }
            self.tex.borrow()[f]
        }
        fn set_face_texture(&self, _node: Node, face: &MeshHandle, set: u32, tex: &FaceTex) {
            let f = (unkey(face) - 0x100) as usize;
            self.log(format!("set_tex f={f} set={set}"));
            if self.drop_set {
                return;
            }
            self.tex.borrow_mut()[f] = *tex;
            if self.nan_after_set == Some(f) {
                self.tex_nan.borrow_mut().push(f);
            }
        }
        fn align_face_textures(&self, _node: Node, faces: &[MeshHandle], set: u32, mode: i32, flags: i32) {
            self.log(format!("align n={} set={set} mode={mode} flags={flags}", faces.len()));
            for h in faces {
                let f = (unkey(h) - 0x100) as usize;
                let mut t = self.tex.borrow_mut();
                t[f].u = [1.0, 0.0, 0.0, 0.0];
                t[f].v = [0.0, -1.0, 0.0, 0.0];
                if self.align_offset {
                    t[f].u[3] = 7.0;
                }
                if flags & 8 != 0 {
                    t[f].scale = [0.25, 0.25];
                }
            }
            if self.nan_after_write {
                self.tex_nan.borrow_mut().push(0);
            }
        }
        fn justify_face_textures(&self, _node: Node, faces: &[MeshHandle], set: u32, mode: u32) {
            self.log(format!("justify n={} set={set} mode={mode}", faces.len()));
            if self.nan_after_write {
                self.tex_nan.borrow_mut().push(0);
            }
        }
        fn material_name(&self, _node: Node, index: i32) -> Vec<u8> {
            assert!(index >= 0, "MATERIAL_NAME is never called with < 0");
            self.log(format!("material_name {index}"));
            self.topo.as_ref().expect("topology").names[index as usize].as_bytes().to_vec()
        }
    }

    /// The [`Write`] half of the fake: the mesh bodies reach only the four C primitives `Mesh`
    /// inherits (`undo_open`, `record_node_tree`, `SetAngles`, `SetScales`); every other method is
    /// a loud failure, so a body that starts calling a new write primitive shows up here.
    impl Write for Fake {
        fn class_exists(&self, _cls: &[u8]) -> bool {
            unreachable!("the mesh lane never looks a class up")
        }
        fn create_node(&self, _doc: Doc, _target: &mut CreateTarget) -> Option<Node> {
            unreachable!("the mesh lane creates no node by name")
        }
        fn properties(&self, _node: Node) -> Option<usize> {
            unreachable!("the mesh lane reads no entity properties")
        }
        fn undo_open(&self, _doc: Doc, label: &'static CStr) -> usize {
            self.log(format!("undo_open {}", String::from_utf8_lossy(label.to_bytes())));
            HISTORY
        }
        fn record_node_tree(&self, hist: usize, node: Node) {
            self.log(format!("record_node_tree {hist:#x} {:#x}", node.0));
        }
        fn doc_set_modified(&self, _doc: Doc) {
            unreachable!("the mesh lane does not set the modified flag")
        }
        fn get_active_history(&self) -> usize {
            HISTORY
        }
        fn get_redo_history(&self, _doc: Doc) -> usize {
            unreachable!("the mesh lane reads no redo history")
        }
        fn history_ok(&self, _hist: usize) -> bool {
            unreachable!("the mesh lane checks no history identity")
        }
        fn history_info(&self, _hist: usize) -> HistoryInfo {
            unreachable!("the mesh lane reads no history info")
        }
        fn undo_pop(&self, _hist: usize) {
            unreachable!("the mesh lane pops no undo")
        }
        fn node_locked(&self, _node: Node) -> bool {
            unreachable!("the mesh lane checks no lock")
        }
        fn delete_node(&self, _doc: Doc, _node: Node) {
            unreachable!("the mesh lane deletes no node")
        }
        fn set_origin(&self, _node: Node, _value: [f32; 3]) {
            unreachable!("the mesh lane sets no origin")
        }
        fn set_angles(&self, _node: Node, value: [f32; 3]) {
            self.log(format!("set_angles {value:?}"));
        }
        fn set_scales(&self, _node: Node, value: [f32; 3]) {
            self.log(format!("set_scales {value:?}"));
        }
        fn angles(&self, _node: Node) -> Option<[f32; 3]> {
            unreachable!("the mesh lane reads `m_Angles` directly (`:4695`)")
        }
        fn scales(&self, _node: Node) -> Option<[f32; 3]> {
            unreachable!("the mesh lane reads `m_Scales` directly (`:4691`)")
        }
        fn attr_previous(&self, _element: usize, _name: &[u8]) -> PrevValue {
            unreachable!("the mesh lane reads no attribute")
        }
        fn attr_info(&self, _element: usize, _name: &[u8]) -> AttrInfo {
            unreachable!("the mesh lane reads no attribute")
        }
        fn case_variant(&self, _element: usize, _name: &[u8]) -> Option<Vec<u8>> {
            unreachable!("the mesh lane looks up no key")
        }
        fn set_key_value(&self, _node: Node, _name: &[u8], _value: &[u8]) {
            unreachable!("the mesh lane writes no keyvalue")
        }
        fn set_value_int(&self, _attr: usize, _value: i32) {
            unreachable!("the mesh lane writes no int attribute")
        }
        fn set_value_float(&self, _attr: usize, _value: f32) {
            unreachable!("the mesh lane writes no float attribute")
        }
        fn conn_count(&self, _node: Node) -> Option<usize> {
            unreachable!("the mesh lane reads no connections")
        }
        fn conn_entry(&self, _node: Node, _index: usize) -> Option<Conn> {
            unreachable!("the mesh lane reads no connections")
        }
        fn conn_filter_fields(&self, _node: Node, _index: usize) -> Option<FilterFields> {
            unreachable!("the mesh lane reads no connections")
        }
        fn add_connection(&self, _node: Node, _spec: &ConnSpec) -> (bool, Option<i32>) {
            unreachable!("the mesh lane adds no connection")
        }
        fn remove_connection(&self, _node: Node, _index: i32) -> bool {
            unreachable!("the mesh lane removes no connection")
        }
    }

    /// `,"expectDoc":{"id":"dN","epoch":N}` with the process's current fingerprint, so the
    /// gate's `expect_doc_ok` passes whatever another test left in the static.
    fn expect_doc() -> String {
        let (seq, epoch) = crate::ops::hammer::fingerprint_ids();
        format!(r#","expectDoc":{{"id":"d{seq}","epoch":{epoch}}}"#)
    }

    fn create_params() -> String {
        format!(
            r#"{{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3]],"material":"materials/dev/x.vmat"{}}}"#,
            expect_doc()
        )
    }

    fn geometry_params() -> String {
        format!(
            r#"{{"nodeID":42,"vertices":[[0,0,0],[4,0,0],[4,4,0],[0,4,0]],"faces":[[0,1,2,3]],"material":"materials/dev/y.vmat"{}}}"#,
            expect_doc()
        )
    }

    fn material_params() -> String {
        format!(
            r#"{{"nodeID":42,"material":"materials/tools/toolsnodraw.vmat"{}}}"#,
            expect_doc()
        )
    }

    fn run(op: GeoOp, fake: &Fake, params: &str) -> String {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op(fake, Doc(0x4000), Some(params), &mut out)
            .map(|()| String::from_utf8_lossy(out.as_cstr()).into_owned())
            .unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn run_err(op: GeoOp, fake: &Fake, params: Option<&str>) -> OpErr {
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        op(fake, Doc(0x4000), params, &mut out).expect_err("the op must refuse")
    }

    // -----------------------------------------------------------------------------------------
    // The parser (`geom_parse`, `mcp_bridge.c:3188-3271`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn geom_parse_reads_the_c_shape() {
        let params = br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3]],"material":"materials/dev/x.vmat"}"#;
        let g = geom_parse(params).expect("the C accepts this shape");
        assert_eq!(g.nv, 4);
        assert_eq!(g.nf, 2);
        assert_eq!(g.nidx, 6);
        assert_eq!(&g.v[..4], &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]]);
        // :3226/:3239/:3241: fstart/fcount/fmat per face, default material slot -1.
        assert_eq!(&g.fstart[..2], &[0, 3]);
        assert_eq!(&g.fcount[..2], &[3, 3]);
        assert_eq!(&g.fmat[..2], &[-1, -1]);
        assert_eq!(&g.idx[..6], &[0, 1, 2, 0, 2, 3]);
        assert_eq!(g.nmat, 0);
        assert_eq!(g.defmat, b"materials/dev/x.vmat");
    }

    #[test]
    fn geom_parse_refuses_every_malformed_member() {
        // Every message is the C's `*why` (`:3197-3269`), reached in the C's order.
        let cases: &[(&[u8], &str)] = &[
            (br#"{"faces":[[0,1,2]],"material":"m"}"#, "vertices: [[x,y,z],...] required"),
            (br#"{"vertices":{},"faces":[[0,1,2]],"material":"m"}"#, "vertices: [[x,y,z],...] required"),
            (br#"{"vertices":[[0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m"}"#, "vertices: each must be [x,y,z]"),
            (br#"{"vertices":[[0,0,x],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m"}"#, "vertices: each must be [x,y,z] with |coord| < 1e6"),
            (br#"{"vertices":[[0,0,1000000],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m"}"#, "vertices: each must be [x,y,z] with |coord| < 1e6"),
            (br#"{"vertices":[[0,0,0],[1,0,0]],"faces":[[0,1,0]],"material":"m"}"#, "vertices: at least 3 [x,y,z]"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]]}"#, "faces: [[i,j,k,...],...] required"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,3]],"material":"m"}"#, "faces: vertex indices must be integers in range"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,1.5]],"material":"m"}"#, "faces: vertex indices must be integers in range"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1]],"material":"m"}"#, "faces: each needs at least 3 vertices"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0 1 2]],"material":"m"}"#, "faces: malformed"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[],"material":"m"}"#, "faces: at least one face"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]]}"#, "material: a material path, e.g. materials/dev/reflectivity_30.vmat"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":""}"#, "material: a material path, e.g. materials/dev/reflectivity_30.vmat"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":["a","b"]}"#, "faceMaterials: longer than faces"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":[true]}"#, "faceMaterials: strings or null"),
            (br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":["a" "b"]}"#, "faceMaterials: malformed"),
        ];
        for (json, why) in cases {
            assert_eq!(geom_parse(json).unwrap_err(), *why, "{}", String::from_utf8_lossy(json));
        }
    }

    #[test]
    fn geom_parse_face_materials_slots() {
        // :3250-3271: null, the default material's own spelling and a distinct material.
        let params = br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3],[0,1,3]],"material":"materials/dev/x.vmat","faceMaterials":[null,"materials/dev/x.vmat","materials/b.vmat"]}"#;
        let g = geom_parse(params).expect("the C accepts this shape");
        assert_eq!(g.nmat, 1);
        assert_eq!(g.mats, vec![b"materials/b.vmat".to_vec()]);
        assert_eq!(&g.fmat[..3], &[-1, -1, 0]);

        // The same material twice is one slot; the 65th distinct one is the C's refusal.
        let mut json = String::from(
            r#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2],[0,1,2]],"material":"d","faceMaterials":["#,
        );
        json.push_str(r#""m1","m1"]}"#);
        let g = geom_parse(json.as_bytes()).expect("two faces, one material");
        assert_eq!(g.nmat, 1);
        assert_eq!(g.fmat[..2], [0, 0]);

        let mut json = String::from(
            r#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":["#,
        );
        let mut faces = String::new();
        for i in 0..65 {
            if i != 0 {
                faces.push(',');
            }
            faces.push_str("[0,1,2]");
        }
        json.push_str(&faces);
        json.push_str(r#"]],"material":"d","faceMaterials":["#);
        for i in 0..65 {
            if i != 0 {
                json.push(',');
            }
            json.push_str(&format!("\"m{i}\""));
        }
        json.push_str("]}");
        assert_eq!(geom_parse(json.as_bytes()).unwrap_err(), "faceMaterials: at most 64 distinct materials");
    }

    #[test]
    fn geom_parse_caps_and_face_material_shapes() {
        // A faceMaterials member that is not an array is ignored (`:3251`: `*p == '['`).
        let g = geom_parse(
            br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":null}"#,
        )
        .expect("accepted");
        assert_eq!(g.nmat, 0);
        assert_eq!(g.fmat[0], -1);

        // :3256-3257: only the four bytes `null` are skipped.
        assert_eq!(
            geom_parse(
                br#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":[nul]}"#
            )
            .unwrap_err(),
            "faceMaterials: strings or null"
        );

        // A material longer than 259 bytes fails `jstr`'s cap (`:3166-3167`).
        let long = "x".repeat(260);
        let json = format!(
            r#"{{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":[[0,1,2]],"material":"m","faceMaterials":["{long}"]}}"#
        );
        assert_eq!(
            geom_parse(json.as_bytes()).unwrap_err(),
            "faceMaterials: strings or null"
        );
    }

    #[test]
    fn geom_parse_refuses_the_three_caps() {
        // :3200 `g->nv >= GEO_MAX_VERTS`: the 65537th vertex.
        let mut json = String::from(r#"{"vertices":["#);
        for i in 0..=GEO_MAX_VERTS {
            if i != 0 {
                json.push(',');
            }
            json.push_str("[0,0,0]");
        }
        json.push_str(r#"],"faces":[[0,1,2]],"material":"m"}"#);
        assert_eq!(
            geom_parse(json.as_bytes()).unwrap_err(),
            "too many vertices (max 65536)"
        );

        // :3225 `g->nf >= GEO_MAX_FACES`: the 65537th face.
        let mut json = String::from(r#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":["#);
        for i in 0..=GEO_MAX_FACES {
            if i != 0 {
                json.push(',');
            }
            json.push_str("[0,1,2]");
        }
        json.push_str(r#"],"material":"m"}"#);
        assert_eq!(
            geom_parse(json.as_bytes()).unwrap_err(),
            "too many faces (max 65536)"
        );

        // :3232 `g->nidx >= GEO_MAX_IDX` (1 << 20): 58255 faces of 18 indices pass the face
        // cap and overrun the index budget on the 1048577th index.
        let mut json = String::from(r#"{"vertices":[[0,0,0],[1,0,0],[1,1,0]],"faces":["#);
        let face = "[0,1,2,0,1,2,0,1,2,0,1,2,0,1,2,0,1,2]";
        for i in 0..GEO_MAX_IDX / 18 + 1 {
            if i != 0 {
                json.push(',');
            }
            json.push_str(face);
        }
        json.push_str(r#"],"material":"m"}"#);
        assert_eq!(
            geom_parse(json.as_bytes()).unwrap_err(),
            "too many face indices"
        );
    }

    #[test]
    fn jstr_accepts_only_the_three_escapes() {
        let mut out = [0u8; 260];
        assert_eq!(jstr(br#" "a\"b\\c\/d"x"#, 0, &mut out), Some(13));
        assert_eq!(cstr(&out), br#"a"b\c/d"#);
        assert_eq!(jstr(br#""a\nb""#, 0, &mut out), None);
        assert_eq!(jstr(br#"a"#, 0, &mut out), None);
        assert_eq!(jstr(br#""unterminated"#, 0, &mut out), None);
        // 259 characters fit, 260 do not.
        let ok = format!("\"{}\"", "x".repeat(259));
        assert!(jstr(ok.as_bytes(), 0, &mut out).is_some());
        let long = format!("\"{}\"", "x".repeat(260));
        assert_eq!(jstr(long.as_bytes(), 0, &mut out), None);
    }

    // -----------------------------------------------------------------------------------------
    // The reply builders (`geo_mesh_summary` + the three tails)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn create_reply_is_the_c_sb_add_sequence() {
        // mcp_bridge.c:5407-5415 read through sb_add/sb_addi/sb_addf3: braces, the summary,
        // inputVertices, faces, materials (nmat + 1) and the label.
        let summary = MeshSummary {
            node_id: 42,
            origin: [1.0, 2.5, -3.0],
            vertex_slots: 8,
            face_slots: 6,
        };
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        create_reply_into(&mut out, &summary, 4, 1, 1);
        assert_eq!(
            String::from_utf8_lossy(out.as_cstr()),
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"inputVertices":4,"faces":1,"materials":1,"undoLabel":"MCP: create mesh"}"#
        );
    }

    #[test]
    fn set_geometry_reply_is_the_c_sb_add_sequence() {
        // mcp_bridge.c:5438-5446.
        let summary = MeshSummary {
            node_id: -1,
            origin: [0.0, 0.0, 0.0],
            vertex_slots: 0,
            face_slots: 0,
        };
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        set_geometry_reply_into(&mut out, &summary, 12, 7);
        assert_eq!(
            String::from_utf8_lossy(out.as_cstr()),
            r#"{"nodeID":-1,"class":"mesh","origin":[0,0,0],"vertexSlots":0,"faceSlots":0,"inputVertices":12,"faces":7,"undoLabel":"MCP: set mesh geometry"}"#
        );
    }

    #[test]
    fn set_material_reply_is_the_c_sb_add_sequence() {
        // mcp_bridge.c:5467-5472: `material` goes through sb_addjson (`:5471`).
        let summary = MeshSummary {
            node_id: 7,
            origin: [320.0, -448.0, 64.0],
            vertex_slots: 8,
            face_slots: 6,
        };
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        set_material_reply_into(&mut out, &summary, 6, b"materials/tools/toolsnodraw.vmat");
        assert_eq!(
            String::from_utf8_lossy(out.as_cstr()),
            r#"{"nodeID":7,"class":"mesh","origin":[320,-448,64],"vertexSlots":8,"faceSlots":6,"faces":6,"material":"materials/tools/toolsnodraw.vmat","undoLabel":"MCP: set mesh material"}"#
        );
    }

    #[test]
    fn mesh_summary_keeps_the_c_defaults() {
        // :4737-4740: id -1 and origin zeros survive a failed read.
        let mut fake = Fake::mesh();
        fake.node_id = None;
        fake.origin = None;
        let summary = mesh_summary(&fake, NODE);
        assert_eq!(
            summary,
            MeshSummary {
                node_id: -1,
                origin: [0.0, 0.0, 0.0],
                vertex_slots: 8,
                face_slots: 6,
            }
        );
        let mut buf = vec![0u8; 256];
        let mut out = Sb::new(&mut buf);
        mesh_summary_into(&mut out, &summary);
        assert_eq!(
            String::from_utf8_lossy(out.as_cstr()),
            r#""nodeID":-1,"class":"mesh","origin":[0,0,0],"vertexSlots":8,"faceSlots":6"#
        );
    }

    // -----------------------------------------------------------------------------------------
    // The op bodies (`mcp_bridge.c:5392-5474`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn mesh_create_calls_in_the_c_order() {
        let fake = Fake::mesh();
        let reply = run(op_mesh_create, &fake, &create_params());
        assert_eq!(
            reply,
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"inputVertices":4,"faces":2,"materials":1,"undoLabel":"MCP: create mesh"}"#
        );
        // :5396-5404: root, group, element, identity, the polygons, then insert + record.
        let calls = fake.take();
        let want = vec![
            "root_file_id".to_string(),
            "undo_open MCP: create mesh".to_string(),
            "create file=7 name=0 src=mcp_bridge line=0".to_string(),
            "is_mesh 0xa000".to_string(),
            "init nf=2 npts=6 mat=materials/dev/x.vmat cap=2".to_string(),
            "insert 0xa000".to_string(),
            "record_node 0x7000 0xa000".to_string(),
        ];
        assert_eq!(&calls[..7], want.as_slice());
    }

    #[test]
    fn mesh_create_assigns_each_face_material_slot() {
        let params = format!(
            r#"{{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3]],"material":"materials/dev/x.vmat","faceMaterials":["materials/b.vmat",null]{}}}"#,
            expect_doc()
        );
        let fake = Fake::mesh();
        let reply = run(op_mesh_create, &fake, &params);
        assert!(reply.ends_with(r#""materials":2,"undoLabel":"MCP: create mesh"}"#), "{reply}");
        let calls = fake.take();
        // :4719-4728: only faces whose fmat matches the slot, one call per distinct material.
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("assign"))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["assign n=1 mat=materials/b.vmat"]
        );
    }

    #[test]
    fn mesh_create_maps_each_material_slot_to_the_created_faces() {
        // :4719-4728: `sel[n++] = fh[f]` - the handle `MeshInitFromPolygons` created for face
        // `f`, not the slot number. The fake hands out `[0xA0 + face; 16]` per face.
        let params = format!(
            r#"{{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3],[0,1,3]],"material":"materials/dev/x.vmat","faceMaterials":["materials/b.vmat",null,"materials/c.vmat"]{}}}"#,
            expect_doc()
        );
        let fake = Fake::mesh();
        let reply = run(op_mesh_create, &fake, &params);
        assert!(reply.ends_with(r#""materials":3,"undoLabel":"MCP: create mesh"}"#), "{reply}");
        let assigns = fake.assigns.borrow();
        assert_eq!(assigns.len(), 2, "one call per distinct material slot");
        // Slot 0 is face 0's handle; slot 1 (b.vmat) is face 2's. The null face keeps the default.
        assert_eq!(assigns[0].1, b"materials/b.vmat");
        assert_eq!(assigns[0].0, vec![[0xA0u8; 16]]);
        assert_eq!(assigns[1].1, b"materials/c.vmat");
        assert_eq!(assigns[1].0, vec![[0xA2u8; 16]]);
    }

    #[test]
    fn mesh_create_skips_materials_when_the_created_count_differs() {
        // :4719 `if (g->nmat && out.size == g->nf)`: Hammer created fewer faces than the input,
        // so the C assigns nothing at all (the face/handle indices would not pair up).
        let params = format!(
            r#"{{"vertices":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"faces":[[0,1,2],[0,2,3]],"material":"materials/dev/x.vmat","faceMaterials":["materials/b.vmat",null]{}}}"#,
            expect_doc()
        );
        let mut fake = Fake::mesh();
        fake.init_size = Some(1);
        let reply = run(op_mesh_create, &fake, &params);
        assert!(reply.contains(r#""faces":1"#), "{reply}");
        assert!(fake.assigns.borrow().is_empty(), "no AssignMaterialToFaces call");
        assert!(!fake.take().iter().any(|c| c.starts_with("assign")));
    }

    #[test]
    fn mesh_create_refuses_when_hammer_created_no_faces() {
        // :4732-4733: `out.size <= 0` is geo_build's own reason, after the element exists.
        let mut fake = Fake::mesh();
        fake.init_size = Some(0);
        let err = run_err(op_mesh_create, &fake, Some(&create_params()));
        assert_eq!(err, OpErr::new(-32603, "InternalError", "Hammer created no faces (degenerate polygons?)"));
        let calls = fake.take();
        assert!(!calls.iter().any(|c| c.starts_with("insert")), "no insert after a failed build");
    }

    #[test]
    fn mesh_create_validates_params_before_any_game_access() {
        let fake = Fake::mesh();
        // :5393-5395: no params is the C's literal "params" reason.
        assert_eq!(
            run_err(op_mesh_create, &fake, None),
            OpErr::new(-32602, "InvalidParams", "params")
        );
        assert_eq!(
            run_err(op_mesh_create, &fake, Some(r#"{"vertices":[]}"#)),
            OpErr::new(
                -32602,
                "InvalidParams",
                "vertices: at least 3 [x,y,z]"
            )
        );
        assert!(fake.take().is_empty(), "the parser runs before the document is read");
    }

    #[test]
    fn mesh_create_refuses_a_missing_root() {
        // :5396-5397, after the parse.
        let mut fake = Fake::mesh();
        fake.file_id = None;
        assert_eq!(
            run_err(op_mesh_create, &fake, Some(&create_params())),
            OpErr::new(-32603, "InternalError", "no document root")
        );
        assert_eq!(fake.take(), vec!["root_file_id".to_string()]);
    }

    #[test]
    fn mesh_create_refuses_a_non_mesh_element() {
        // :5400: CreateElement's answer that is not a CMapMesh.
        let mut fake = Fake::mesh();
        fake.mesh_class = false;
        assert_eq!(
            run_err(op_mesh_create, &fake, Some(&create_params())),
            OpErr::new(-32603, "InternalError", "CreateElement<CMapMesh> failed")
        );
    }

    #[test]
    fn geo_target_validates_the_node_id_forms() {
        // :5417-5420: every malformed nodeID is the same InvalidParams, before the tree walk.
        for params in [
            r#"{}"#,
            r#"{"nodeID":-1}"#,
            r#"{"nodeID":4294967326}"#,
            r#"{"nodeID":"42"}"#,
            r#"{"nodeID":1.5}"#,
        ] {
            let fake = Fake::mesh();
            let err = run_err(op_mesh_set_material, &fake, Some(params));
            assert_eq!(
                err,
                OpErr::new(
                    -32602,
                    "InvalidParams",
                    "nodeID: a non-negative int up to 2147483647 is required"
                ),
                "{params}"
            );
            assert!(fake.take().is_empty(), "{params}: no game access");
        }
        // :5418: no params at all is the same refusal.
        let fake = Fake::mesh();
        assert_eq!(
            run_err(op_mesh_set_material, &fake, None).code,
            -32602
        );
    }

    #[test]
    fn geo_target_refuses_an_unknown_node_and_a_non_mesh() {
        // :5422-5424.
        let mut fake = Fake::mesh();
        fake.node_id = Some(43);
        assert_eq!(
            run_err(op_mesh_set_material, &fake, Some(&material_params())),
            OpErr::new(
                -32022,
                "NodeNotFound",
                "no node with this nodeID in the live document"
            )
        );
        let mut fake = Fake::mesh();
        fake.mesh_class = false;
        assert_eq!(
            run_err(op_mesh_set_material, &fake, Some(&material_params())),
            OpErr::new(-32602, "InvalidParams", "node is not a mesh")
        );
    }

    #[test]
    fn mesh_set_geometry_resets_only_non_default_transforms() {
        // :4691-4700: scales only when not {1,1,1}, angles only when not {0,0,0}, pivot always.
        let mut fake = Fake::mesh();
        fake.scales = [2.0, 2.0, 2.0];
        fake.angles = [0.0, 0.0, 0.0];
        let reply = run(op_mesh_set_geometry, &fake, &geometry_params());
        assert_eq!(
            reply,
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"inputVertices":4,"faces":1,"undoLabel":"MCP: set mesh geometry"}"#
        );
        let calls = fake.take();
        let pos = |needle: &str| calls.iter().position(|c| c.as_str() == needle);
        // :5429-5437: the target's identity check, then group, record, clear, build.
        assert_eq!(pos("is_mesh 0x9000"), Some(0), "{calls:?}");
        assert_eq!(pos("undo_open MCP: set mesh geometry"), Some(1), "{calls:?}");
        assert_eq!(pos("record_node_tree 0x7000 0x9000"), Some(2), "{calls:?}");
        assert_eq!(pos("collect cap=6"), Some(3), "{calls:?}");
        assert_eq!(pos("delete size=6 alloc=6 buf=6"), Some(4), "{calls:?}");
        assert_eq!(pos("set_scales [1.0, 1.0, 1.0]"), Some(5), "{calls:?}");
        assert!(pos("set_angles [0.0, 0.0, 0.0]").is_none(), "angles already zero: {calls:?}");
        assert_eq!(pos("set_pivot [0.0, 0.0, 0.0]"), Some(6), "{calls:?}");
        assert_eq!(pos("init nf=1 npts=4 mat=materials/dev/y.vmat cap=1"), Some(7), "{calls:?}");
    }

    #[test]
    fn mesh_set_geometry_refuses_bad_geometry_and_a_failed_build() {
        // :5431-5433: the parse runs after the target resolved (`is_mesh` is the target's own
        // check), and its `why` is the InvalidParams data.
        let fake = Fake::mesh();
        let params = format!(
            r#"{{"nodeID":42,"material":"m"{}}}"#,
            expect_doc()
        );
        assert_eq!(
            run_err(op_mesh_set_geometry, &fake, Some(&params)),
            OpErr::new(
                -32602,
                "InvalidParams",
                "vertices: [[x,y,z],...] required"
            )
        );
        assert_eq!(fake.take()[0], "is_mesh 0x9000");

        // :5437: a reset mesh that builds no faces is geo_build's `why`.
        let mut fake = Fake::mesh();
        fake.init_size = Some(0);
        assert_eq!(
            run_err(op_mesh_set_geometry, &fake, Some(&geometry_params())),
            OpErr::new(
                -32603,
                "InternalError",
                "Hammer created no faces (degenerate polygons?)"
            )
        );
    }

    #[test]
    fn mesh_set_geometry_writes_angles_when_rotated() {
        let mut fake = Fake::mesh();
        fake.angles = [0.0, 90.0, 0.0];
        run(op_mesh_set_geometry, &fake, &geometry_params());
        let calls = fake.take();
        assert!(calls.contains(&"set_angles [0.0, 0.0, 0.0]".to_string()), "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("set_scales")), "{calls:?}");
    }

    #[test]
    fn mesh_set_geometry_refuses_a_mesh_that_cannot_be_cleared() {
        // :4676: the C's face-count ceiling; :5436: the missing buffer/collection.
        let mut fake = Fake::mesh();
        fake.face_slots = GEO_FACE_CAP + 1;
        assert_eq!(
            run_err(op_mesh_set_geometry, &fake, Some(&geometry_params())),
            OpErr::new(-32603, "InternalError", "could not clear the mesh")
        );
        let mut fake = Fake::mesh();
        fake.collect_fails = true;
        assert_eq!(
            run_err(op_mesh_set_geometry, &fake, Some(&geometry_params())),
            OpErr::new(-32603, "InternalError", "could not clear the mesh")
        );
    }

    #[test]
    fn geo_reset_mesh_hands_delete_an_alloc_handle_buffer() {
        // :4677-4681: the C allocates `n` handles and sets `v.alloc = n` before CollectHandles
        // fills `v.size <= n`; MeshDeleteFaces is handed that whole block. CollectHandles
        // returns 4 of the 6 faces here, and the buffer must still be 6 handles long.
        let mut fake = Fake::mesh();
        fake.face_slots = 6;
        fake.collect_size = Some(4);
        run(op_mesh_set_geometry, &fake, &geometry_params());
        let calls = fake.take();
        assert!(
            calls.contains(&"delete size=4 alloc=6 buf=6".to_string()),
            "{calls:?}"
        );

        // The buffer itself: the collected handles at the front, zeroed tail, `size` = 4.
        let collected = vec![[0x11u8; 16]; 4];
        let (buffer, size) = delete_buffer(&collected, 6);
        assert_eq!(buffer.len(), 6);
        assert_eq!(size, 4);
        assert_eq!(&buffer[..4], &collected[..]);
        assert_eq!(&buffer[4..], &[[0u8; 16]; 2][..]);
        // A short allocation never indexes past either input.
        let (buffer, size) = delete_buffer(&collected, 2);
        assert_eq!(buffer.len(), 2);
        assert_eq!(size, 2);
    }

    #[test]
    fn mesh_set_material_collects_before_the_group() {
        // :5455-5465: counts, CollectHandles, then the group, the record and the assign.
        let fake = Fake::mesh();
        let reply = run(op_mesh_set_material, &fake, &material_params());
        assert_eq!(
            reply,
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"faces":6,"material":"materials/tools/toolsnodraw.vmat","undoLabel":"MCP: set mesh material"}"#
        );
        assert_eq!(
            fake.take(),
            vec![
                "is_mesh 0x9000".to_string(),
                "collect cap=6".to_string(),
                "undo_open MCP: set mesh material".to_string(),
                "record_node_tree 0x7000 0x9000".to_string(),
                "assign n=6 mat=materials/tools/toolsnodraw.vmat".to_string(),
            ]
        );
    }

    #[test]
    fn mesh_set_material_refusals() {
        // :5453-5454: a missing/empty material, after the node resolved.
        let fake = Fake::mesh();
        assert_eq!(
            run_err(
                op_mesh_set_material,
                &fake,
                Some(&format!(r#"{{"nodeID":42{}}}"#, expect_doc()))
            ),
            OpErr::new(-32602, "InvalidParams", "material required")
        );
        // :5456: no faces.
        let mut fake = Fake::mesh();
        fake.face_slots = 0;
        assert_eq!(
            run_err(op_mesh_set_material, &fake, Some(&material_params())),
            OpErr::new(-32603, "InternalError", "mesh has no faces")
        );
        // :5460-5462: CollectHandles refused, and the empty vector.
        let mut fake = Fake::mesh();
        fake.collect_fails = true;
        assert_eq!(
            run_err(op_mesh_set_material, &fake, Some(&material_params())),
            OpErr::new(-32603, "InternalError", "could not collect face handles")
        );
        let mut fake = Fake::mesh();
        fake.collect_size = Some(0);
        assert_eq!(
            run_err(op_mesh_set_material, &fake, Some(&material_params())),
            OpErr::new(-32603, "InternalError", "could not collect face handles")
        );
    }

    // -----------------------------------------------------------------------------------------
    // The dispatch arm (`geo_dispatch`, `mcp_bridge.c:5612-5659`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn geo_dispatch_appends_the_doc_tail_before_the_overflow_check() {
        let fake = Fake::mesh();
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        let reply = geo_dispatch(
            &fake,
            "t1",
            Some(&create_params()),
            &mut out,
            op_mesh_create,
        );
        // :5649-5652: the tail is spliced into the result object, then the envelope.
        let (seq, _) = crate::ops::hammer::fingerprint_ids();
        let expected = format!(
            r#"{{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"inputVertices":4,"faces":2,"materials":1,"undoLabel":"MCP: create mesh","doc":{{"id":"d{seq}","topSerial":0,"undoDepth":0}}}}"#
        );
        assert_eq!(reply, crate::control::result_ok(Some("t1"), &expected));
    }

    #[test]
    fn geo_dispatch_answers_errors_without_a_doc_tail() {
        let fake = Fake::mesh();
        let mut buf = vec![0u8; OP_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        // The gate runs first (`:5615-5645`): an `expectDoc` without an epoch is `missing_epoch`
        // and answers -32004, and neither a `doc` tail nor a single game call follows.
        let (seq, _) = crate::ops::hammer::fingerprint_ids();
        let reply = geo_dispatch(
            &fake,
            "t2",
            Some(&format!(r#"{{"vertices":[],"expectDoc":{{"id":"d{seq}"}}}}"#)),
            &mut out,
            op_mesh_create,
        );
        assert_eq!(
            reply,
            crate::control::error_ext(
                Some("t2"),
                -32004,
                "DocumentChanged",
                r#"{"reason":"missing_epoch"}"#
            )
        );
        assert!(fake.take().is_empty(), "the body never ran");
        assert!(out.as_cstr().is_empty(), "no result object was built");
    }

    #[test]
    fn geo_dispatch_reports_an_overflow_as_the_c_does() {
        // :5653-5657: the tail is spliced first, then `rb.ovf` turns the whole result into the
        // C's -32603 with the shared overflow text (a small buffer stands in for the 512 KB one).
        let fake = Fake::mesh();
        let mut buf = vec![0u8; 32];
        let mut out = Sb::new(&mut buf);
        let reply = geo_dispatch(
            &fake,
            "t3",
            Some(&create_params()),
            &mut out,
            op_mesh_create,
        );
        assert_eq!(
            reply,
            crate::control::error(Some("t3"), -32603, "response too large (use limit)")
        );
    }

    // -----------------------------------------------------------------------------------------
    // mesh.get
    // -----------------------------------------------------------------------------------------

    fn box_topo() -> Topo {
        let verts = vec![
            [0.0, 0.0, 0.0],
            [4.0, 0.0, 0.0],
            [4.0, 4.0, 0.0],
            [0.0, 4.0, 0.0],
            [0.0, 0.0, 2.0],
            [4.0, 0.0, 2.0],
            [4.0, 4.0, 2.0],
            [0.0, 4.0, 2.0],
        ];
        let faces = vec![
            vec![3, 2, 1, 0],
            vec![4, 5, 6, 7],
            vec![0, 1, 5, 4],
            vec![1, 2, 6, 5],
            vec![2, 3, 7, 6],
            vec![3, 0, 4, 7],
        ];
        Topo { verts, faces, face_mat: vec![0; 6], names: vec!["materials/dev/a.vmat"] }
    }

    fn mesh_fake(topo: Topo) -> Fake {
        let mut fake = Fake::mesh();
        fake.vertex_slots = topo.verts.len() as i32;
        fake.face_slots = topo.faces.len() as i32;
        fake.angles = [0.0, 90.0, 0.0];
        fake.scales = [1.0, 2.0, 1.0];
        fake.topo = Some(topo);
        fake
    }

    fn get_params() -> String {
        format!(r#"{{"nodeID":42{}}}"#, expect_doc())
    }

    fn get_json(fake: &Fake, params: &str) -> serde_json::Value {
        let mut buf = vec![0u8; MESH_GET_BUFFER];
        let mut out = Sb::new(&mut buf);
        let reply = mesh_get_dispatch(fake, "g", Some(params), &mut out);
        let v: serde_json::Value = serde_json::from_str(reply.trim()).expect("json frame");
        v["result"].clone()
    }

    fn get_error(fake: &Fake, params: &str) -> (i64, String) {
        let mut buf = vec![0u8; MESH_GET_BUFFER];
        let mut out = Sb::new(&mut buf);
        let reply = mesh_get_dispatch(fake, "g", Some(params), &mut out);
        let v: serde_json::Value = serde_json::from_str(reply.trim()).expect("json frame");
        assert!(v.get("result").is_none(), "{reply}");
        (v["error"]["code"].as_i64().unwrap(), reply)
    }

    #[test]
    fn mesh_get_reads_a_box() {
        let fake = mesh_fake(box_topo());
        let r = get_json(&fake, &get_params());
        assert_eq!(r["nodeID"], 42);
        assert_eq!(r["class"], "mesh");
        assert_eq!(r["space"], "local");
        assert_eq!(r["origin"], serde_json::json!([1, 2.5, -3]));
        assert_eq!(r["angles"], serde_json::json!([0, 90, 0]));
        assert_eq!(r["scales"], serde_json::json!([1, 2, 1]));
        assert_eq!(r["vertexCount"], 8);
        assert_eq!(r["faceCount"], 6);
        assert_eq!(r["vertices"].as_array().unwrap().len(), 8);
        assert_eq!(r["vertices"][6], serde_json::json!([4, 4, 2]));
        assert_eq!(r["faces"][0], serde_json::json!([3, 2, 1, 0]));
        assert_eq!(r["faces"][5], serde_json::json!([3, 0, 4, 7]));
        assert_eq!(r["faceMaterials"], serde_json::json!([0, 0, 0, 0, 0, 0]));
        assert_eq!(r["materials"], serde_json::json!(["materials/dev/a.vmat"]));
        assert!(r["doc"]["id"].as_str().unwrap().starts_with('d'));
        assert!(r["doc"]["topSerial"].is_number() && r["doc"]["undoDepth"].is_number());
    }

    #[test]
    fn mesh_get_floats_round_trip_exactly() {
        let mut t = box_topo();
        t.verts[0] = [0.1, -1.0 / 3.0, 12345.678];
        let r = get_json(&mesh_fake(t), &get_params());
        let v = &r["vertices"][0];
        assert_eq!(v[0].as_f64().unwrap() as f32, 0.1f32);
        assert_eq!(v[1].as_f64().unwrap() as f32, -1.0f32 / 3.0);
        assert_eq!(v[2].as_f64().unwrap() as f32, 12345.678f32);
    }

    #[test]
    fn mesh_get_walks_an_ngon() {
        let verts: Vec<[f32; 3]> = (0..7).map(|i| [i as f32, (i * i) as f32, 0.0]).collect();
        let t = Topo {
            verts,
            faces: vec![(0..7).collect()],
            face_mat: vec![0],
            names: vec!["m"],
        };
        let r = get_json(&mesh_fake(t), &get_params());
        assert_eq!(r["faces"], serde_json::json!([[0, 1, 2, 3, 4, 5, 6]]));
        assert_eq!(r["vertexCount"], 7);
    }

    #[test]
    fn mesh_get_dedups_materials_in_first_use_order_and_keeps_minus_one() {
        let mut t = box_topo();
        // table: 0=a, 1=b, 2=a again (same name, different table entry); -1 = none.
        t.names = vec!["a", "b", "a"];
        t.face_mat = vec![1, 0, 2, -1, 1, 0];
        let fake = mesh_fake(t);
        let r = get_json(&fake, &get_params());
        assert_eq!(r["materials"], serde_json::json!(["b", "a"]));
        assert_eq!(r["faceMaterials"], serde_json::json!([0, 1, 1, -1, 0, 1]));
        // MATERIAL_NAME ran once per distinct table index that was used, never for -1.
        let names: Vec<_> = fake.take().into_iter().filter(|c| c.starts_with("material_name")).collect();
        assert_eq!(names, vec!["material_name 1", "material_name 0", "material_name 2"]);
    }

    #[test]
    fn mesh_get_keeps_every_vertex_in_list_order_and_maps_loops_by_handle() {
        // vertex 0 is isolated; the face visits 3,2,1: indices are list positions.
        let t = Topo {
            verts: vec![[9.0, 9.0, 9.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [3.0, 0.0, 0.0]],
            faces: vec![vec![3, 2, 1]],
            face_mat: vec![-1],
            names: vec![],
        };
        let fake = mesh_fake(t);
        let r = get_json(&fake, &get_params());
        assert_eq!(r["vertexCount"], 4);
        assert_eq!(
            r["vertices"],
            serde_json::json!([[9, 9, 9], [1, 0, 0], [2, 0, 0], [3, 0, 0]])
        );
        assert_eq!(r["faces"], serde_json::json!([[3, 2, 1]]));
        // one batch position call, none per edge
        let calls = fake.take();
        assert_eq!(calls.iter().filter(|c| c.starts_with("vertex_positions")).count(), 1);
        assert!(calls.contains(&"vertex_positions n=4".to_string()), "{calls:?}");
    }

    #[test]
    fn mesh_get_refuses_a_mesh_without_a_position_stream() {
        let mut fake = mesh_fake(box_topo());
        fake.no_positions = true;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32603);
        assert!(reply.contains("no position stream"), "{reply}");
    }

    #[test]
    fn mesh_get_of_an_empty_mesh_is_empty_arrays() {
        let mut fake = Fake::mesh();
        fake.vertex_slots = 0;
        fake.face_slots = 0;
        let r = get_json(&fake, &get_params());
        for key in ["vertices", "faces", "faceMaterials", "materials"] {
            assert_eq!(r[key], serde_json::json!([]), "{key}");
        }
        assert_eq!(r["vertexCount"], 0);
        assert_eq!(r["faceCount"], 0);
    }

    #[test]
    fn mesh_get_refuses_a_mesh_above_the_limits_with_the_counts() {
        let mut fake = Fake::mesh();
        fake.vertex_slots = (GEO_MAX_VERTS * 4 + 1) as i32;
        fake.face_slots = 10;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602);
        assert!(reply.contains(&format!("{} vertices", GEO_MAX_VERTS * 4 + 1)), "{reply}");
        // The collected counts are held to the limits too.
        let verts = vec![[0.0f32; 3]; GEO_MAX_VERTS + 1];
        let t = Topo { verts, faces: vec![], face_mat: vec![], names: vec![] };
        let mut fake = mesh_fake(t);
        fake.face_slots = 0;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602);
        assert!(reply.contains("\"vertexCount\":65537"), "{reply}");
        assert!(reply.contains("\"maxLineBytes\":1048576"), "{reply}");
    }

    #[test]
    fn mesh_get_refuses_a_non_mesh_an_unknown_node_and_a_bad_expect_doc() {
        let mut fake = Fake::mesh();
        fake.mesh_class = false;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602, "{reply}");
        assert!(reply.contains("node is not a mesh"), "{reply}");

        let fake = mesh_fake(box_topo());
        let (code, _) = get_error(&fake, &format!(r#"{{"nodeID":7{}}}"#, expect_doc()));
        assert_eq!(code, -32022);

        let (code, reply) = get_error(&fake, r#"{"nodeID":42,"expectDoc":{"id":"d999999","epoch":123456}}"#);
        assert_eq!(code, -32004, "{reply}");

        // No expectDoc at all is fine for a read.
        let r = get_json(&fake, r#"{"nodeID":42}"#);
        assert_eq!(r["faceCount"], 6);
    }

    #[test]
    fn mesh_get_guards_a_ring_that_never_closes() {
        let mut fake = mesh_fake(box_topo());
        fake.overrun = true;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32603);
        assert!(reply.contains("overran"), "{reply}");
    }

    #[test]
    fn mesh_get_refuses_an_invalid_first_edge_and_a_vertex_outside_the_list() {
        let mut fake = mesh_fake(box_topo());
        fake.bad_first_edge = true;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32603);
        assert!(reply.contains("no first edge"), "{reply}");

        let mut fake = mesh_fake(box_topo());
        fake.hide_vertex = Some(3);
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32603);
        assert!(reply.contains("not in the vertex list"), "{reply}");
    }

    /// A topology of `faces` n-gons over `corners` vertices (each face the ring 0..corners).
    fn ring_topo(faces: usize, corners: usize) -> Topo {
        Topo {
            verts: (0..corners).map(|i| [i as f32, 0.0, 0.0]).collect(),
            faces: vec![(0..corners).collect(); faces],
            face_mat: vec![-1; faces],
            names: vec![],
        }
    }

    #[test]
    fn mesh_get_refuses_a_reply_that_cannot_fit_one_line_before_the_walk() {
        let mut fake = Fake::mesh();
        fake.vertex_slots = 20_000;
        fake.face_slots = 0;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602, "{reply}");
        let v: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
        let data = &v["error"]["data"];
        assert_eq!(data["reason"], "mesh too large for one reply");
        assert_eq!(data["vertexCount"], 20_000);
        assert_eq!(data["faceCount"], 0);
        assert_eq!(data["maxLineBytes"], 1_048_576);
        // refused before anything was collected or walked
        let calls = fake.take();
        assert!(!calls.iter().any(|c| c.starts_with("collect")), "{calls:?}");
    }

    #[test]
    fn mesh_get_refuses_when_the_corners_make_the_reply_too_big() {
        // 12000 faces of 30 corners: the vertex/face estimate fits, the corner count does not.
        let fake = mesh_fake(ring_topo(12_000, 30));
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602, "{reply}");
        let v: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
        assert_eq!(v["error"]["data"]["reason"], "mesh too large for one reply");
        assert_eq!(v["error"]["data"]["vertexCount"], 30);
        assert_eq!(v["error"]["data"]["faceCount"], 12_000);
    }

    #[test]
    fn mesh_get_reports_a_legitimate_mesh_above_the_corner_cap_as_too_large() {
        // 20000 faces x 60 corners = 1.2M corners > GEO_MAX_IDX: the -32602 refusal with counts,
        // not "edge ring does not close".
        let fake = mesh_fake(ring_topo(20_000, 60));
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32602, "{reply}");
        assert!(reply.contains("face corners"), "{reply}");
        assert!(reply.contains("\"faceCount\":20000"), "{reply}");
        assert!(!reply.contains("does not close"), "{reply}");
    }

    #[test]
    fn mesh_get_serves_a_big_mesh_that_fits_one_line() {
        // far above the old 512 KB cap's ~8-10k vertices, inside one 1 MiB line
        let verts: Vec<[f32; 3]> = (0..12_000).map(|i| [i as f32, 0.5, -1.25]).collect();
        let t = Topo { verts, faces: vec![], face_mat: vec![], names: vec![] };
        let mut fake = mesh_fake(t);
        fake.face_slots = 0;
        let r = get_json(&fake, &get_params());
        assert_eq!(r["vertexCount"], 12_000);
        assert_eq!(r["vertices"].as_array().unwrap().len(), 12_000);
    }

    #[test]
    fn mesh_get_refuses_an_unreadable_origin_instead_of_zeroing_it() {
        let mut fake = mesh_fake(box_topo());
        fake.origin = None;
        let (code, reply) = get_error(&fake, &get_params());
        assert_eq!(code, -32603, "{reply}");
        assert!(reply.contains("origin"), "{reply}");
    }

    #[test]
    fn mesh_get_reports_the_parent_chain_and_whether_it_is_world_space() {
        let r = get_json(&mesh_fake(box_topo()), &get_params());
        assert_eq!(r["worldSpace"], true);
        assert_eq!(r["parentChain"], serde_json::json!(["world"]));

        let mut fake = mesh_fake(box_topo());
        fake.chain = vec!["group", "entity"];
        let r = get_json(&fake, &get_params());
        assert_eq!(r["worldSpace"], true);
        assert_eq!(r["parentChain"], serde_json::json!(["group", "entity", "world"]));

        for bad in ["instance", "prefab", "other"] {
            let mut fake = mesh_fake(box_topo());
            fake.chain = vec!["group", bad];
            let r = get_json(&fake, &get_params());
            assert_eq!(r["worldSpace"], false, "{bad}");
            assert_eq!(r["parentChain"], serde_json::json!(["group", bad, "world"]));
        }

        let mut fake = mesh_fake(box_topo());
        fake.parent_broken = true;
        let r = get_json(&fake, &get_params());
        assert_eq!(r["worldSpace"], false, "an unreadable parent is never assumed to be the world");
        assert_eq!(r["parentChain"], serde_json::json!([]));
    }

    #[test]
    fn mesh_get_is_read_only() {
        let fake = mesh_fake(box_topo());
        let before = crate::ops::hammer::fingerprint_ids();
        let _ = get_json(&fake, &get_params());
        let after = crate::ops::hammer::fingerprint_ids();
        assert_eq!(before, after, "the fingerprint is unchanged");
        let calls = fake.take();
        for forbidden in ["undo_open", "record", "set_", "delete", "assign", "init", "create"] {
            assert!(
                !calls.iter().any(|c| c.starts_with(forbidden)),
                "mesh.get made a write-shaped call `{forbidden}`: {calls:?}"
            );
        }
    }

    #[test]
    fn mesh_get_is_an_implemented_hammer_row() {
        let row = crate::registry::find("mesh.get").expect("row");
        assert!(row.implemented && row.blocked_on.is_none());
        assert_eq!(row.needs, crate::registry::NEED_HAMMER);
        assert!(HANDLERS.iter().any(|(name, _)| *name == "mesh.get"));
    }

    // -----------------------------------------------------------------------------------------
    // mesh.setFaceTexture (WP1.5)
    // -----------------------------------------------------------------------------------------

    fn base_tex(i: usize) -> FaceTex {
        FaceTex {
            u: [0.0, 1.0, 0.0, 3.0 + i as f32],
            v: [0.0, 0.0, -1.0, 5.0],
            scale: [0.5, 0.5],
        }
    }

    fn tex_fake() -> Fake {
        let fake = mesh_fake(box_topo());
        *fake.tex.borrow_mut() = (0..6).map(base_tex).collect();
        fake
    }

    fn tex_params(body: &str) -> String {
        format!(r#"{{"nodeID":42,{body}{}}}"#, expect_doc())
    }

    #[test]
    fn tex_parse_accepts_every_mode_and_defaults() {
        let p = tex_parse(r#"{"nodeID":1,"mode":"world"}"#).unwrap();
        assert_eq!(
            p,
            TexParams {
                faces: None,
                axes: Vec::new(),
                mode: TexMode::World,
                keep_scale: false,
                shift: None,
                scale: None,
                rotate: None,
                tex_set: 0
            }
        );
        let p = tex_parse(
            r#"{"mode":"justify","justify":"fitV","faces":[2,0],"texSet":1}"#,
        )
        .unwrap();
        assert_eq!(p.mode, TexMode::Justify(8));
        assert_eq!(p.faces, Some(vec![2, 0]));
        assert_eq!(p.tex_set, 1);
        let p = tex_parse(
            r#"{"mode":"set","shift":[1.5,-2],"scale":[0.25,0.5],"rotate":90,"keepScale":null}"#,
        )
        .unwrap();
        assert_eq!((p.shift, p.scale, p.rotate), (Some([1.5, -2.0]), Some([0.25, 0.5]), Some(90.0)));
        for (name, n) in [("top", 1), ("bottom", 2), ("left", 3), ("center", 4), ("right", 5), ("fit", 6), ("fitU", 7)] {
            let p = tex_parse(&format!(r#"{{"mode":"justify","justify":"{name}"}}"#)).unwrap();
            assert_eq!(p.mode, TexMode::Justify(n), "{name}");
        }
        assert!(tex_parse(r#"{"mode":"face","keepScale":true}"#).unwrap().keep_scale);
    }

    #[test]
    fn tex_parse_refuses_bad_params() {
        let cases: &[(&str, &str)] = &[
            (r#"{}"#, "mode: one of world, face, justify, set, axes is required"),
            (r#"{"mode":"spin"}"#, "mode: one of world, face, justify, set, axes is required"),
            (r#"{"mode":"justify"}"#, "justify: required when mode is \"justify\""),
            (r#"{"mode":"justify","justify":"middle"}"#, "justify: one of left, right, top, bottom, center, fit, fitU, fitV"),
            (r#"{"mode":"world","justify":"left"}"#, "justify: only valid with mode \"justify\""),
            (r#"{"mode":"justify","justify":"left","keepScale":true}"#, "keepScale: only valid with mode \"world\" or \"face\""),
            (r#"{"mode":"world","keepScale":"yes"}"#, "keepScale: a boolean is required"),
            (r#"{"mode":"set"}"#, "mode \"set\" needs at least one of shift, scale, rotate"),
            (r#"{"mode":"set","scale":[0,1]}"#, "scale: both components must be > 0 (world units per texel)"),
            (r#"{"mode":"set","scale":[1,-1]}"#, "scale: both components must be > 0 (world units per texel)"),
            (r#"{"mode":"set","scale":[1]}"#, "scale: [u, v] of two finite numbers required"),
            (r#"{"mode":"set","shift":["a",1]}"#, "shift: [u, v] of two finite numbers required"),
            (r#"{"mode":"set","shift":[1e60,1]}"#, "shift: [u, v] of two finite numbers required"),
            (r#"{"mode":"set","rotate":"90"}"#, "rotate: a number of degrees is required"),
            (r#"{"mode":"set","rotate":1e9}"#, "rotate: degrees, finite, |value| <= 36000"),
            (r#"{"mode":"justify","justify":"fit","scale":[1,1]}"#, "shift, scale and rotate are not valid with mode \"justify\" (they would undo the justify): justify first, then call again with mode \"set\""),
            (r#"{"mode":"justify","justify":"fit","rotate":5}"#, "shift, scale and rotate are not valid with mode \"justify\" (they would undo the justify): justify first, then call again with mode \"set\""),
            (r#"{"mode":"justify","justify":"fit","shift":[1,1]}"#, "shift, scale and rotate are not valid with mode \"justify\" (they would undo the justify): justify first, then call again with mode \"set\""),
            (r#"{"mode":"world","faces":[]}"#, "faces: at least one index (omit the member for every face)"),
            (r#"{"mode":"world","faces":[1,1]}"#, "faces[1]: index 1 is listed twice"),
            (r#"{"mode":"world","faces":[-1]}"#, "faces[0]: a non-negative integer is required"),
            (r#"{"mode":"world","faces":[1.5]}"#, "faces[0]: a non-negative integer is required"),
            (r#"{"mode":"world","faces":"all"}"#, "faces: an array of face indices is required"),
            (r#"{"mode":"world","texSet":3}"#, "texSet: 0 or 1"),
        ];
        for (json, why) in cases {
            assert_eq!(tex_parse(json).unwrap_err(), *why, "{json}");
        }
        // NaN / out-of-range literals are not JSON at all.
        for json in [r#"{"mode":"set","shift":[NaN,1]}"#, r#"{"mode":"set","shift":[1e999,1]}"#] {
            let e = tex_parse(json).unwrap_err();
            assert!(e.starts_with("params: not valid JSON"), "{json}: {e}");
        }
    }

    fn axes_face(index: usize, extra: &str) -> String {
        format!(
            r#"{{"index":{index},"axisU":[1,0,0,{}],"axisV":[0,-1,0,2],"scale":[0.25,0.5]{extra}}}"#,
            index + 1
        )
    }

    #[test]
    fn tex_parse_axes_accepts_and_collects_per_face_axes() {
        let p = tex_parse(&format!(r#"{{"mode":"axes","texSet":1,"faces":[{},{}]}}"#, axes_face(4, ""), axes_face(1, ""))).unwrap();
        assert_eq!(p.mode, TexMode::Axes);
        assert_eq!(p.faces, Some(vec![4, 1]));
        assert_eq!(p.tex_set, 1);
        assert_eq!(p.axes.len(), 2);
        assert_eq!(p.axes[0], FaceTex { u: [1.0, 0.0, 0.0, 5.0], v: [0.0, -1.0, 0.0, 2.0], scale: [0.25, 0.5] });
        // Non-unit axes are accepted: Hammer stores what it is given.
        assert!(tex_parse(r#"{"mode":"axes","faces":[{"index":0,"axisU":[2,0,0,0],"axisV":[1,1,0,0],"scale":[1,1]}]}"#).is_ok());
    }

    #[test]
    fn tex_parse_axes_refuses_bad_params() {
        let f0 = axes_face(0, "");
        let cases: Vec<(String, &str)> = vec![
            (r#"{"mode":"axes"}"#.into(), "faces: an array of {index, axisU, axisV, scale} is required with mode \"axes\""),
            (r#"{"mode":"axes","faces":[]}"#.into(), "faces: at least one {index, axisU, axisV, scale} entry"),
            (r#"{"mode":"axes","faces":[3]}"#.into(), "faces[0]: an object {index, axisU, axisV, scale} is required"),
            (format!(r#"{{"mode":"axes","faces":[{f0},{f0}]}}"#), "faces[1]: index 0 is listed twice"),
            (r#"{"mode":"axes","faces":[{"axisU":[1,0,0,0],"axisV":[0,1,0,0],"scale":[1,1]}]}"#.into(), "faces[0].index: a non-negative integer is required"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisV":[0,1,0,0],"scale":[1,1]}]}"#.into(), "faces[0].axisU: required"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[1,0,0],"axisV":[0,1,0,0],"scale":[1,1]}]}"#.into(), "faces[0].axisU: [x, y, z, offset] of four finite numbers required"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[1,0,0,0],"axisV":[0,1,0,1e60],"scale":[1,1]}]}"#.into(), "faces[0].axisV: [x, y, z, offset] of four finite numbers required"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[1,0,0,0],"axisV":[0,1,0,0],"scale":[0,1]}]}"#.into(), "faces[0].scale: both components must be > 0 (world units per texel)"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[0,0,0,5],"axisV":[0,1,0,0],"scale":[1,1]}]}"#.into(), "faces[0].axisU: the xyz axis must not be zero-length"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[1,0,0,0],"axisV":[0,0,0,0],"scale":[1,1]}]}"#.into(), "faces[0].axisV: the xyz axis must not be zero-length"),
            (r#"{"mode":"axes","faces":[{"index":0,"axisU":[1,0,0,0],"axisV":[-3,0,0,9],"scale":[1,1]}]}"#.into(), "faces[0]: axisU and axisV must not be parallel"),
            (format!(r#"{{"mode":"axes","faces":[{f0}],"texSet":2}}"#), "texSet: 0 or 1"),
            (format!(r#"{{"mode":"axes","faces":[{f0}],"shift":[1,1]}}"#), "shift: not valid with mode \"axes\" (put axisU, axisV and scale inside each faces entry)"),
            (format!(r#"{{"mode":"axes","faces":[{f0}],"scale":[1,1]}}"#), "scale: not valid with mode \"axes\" (put axisU, axisV and scale inside each faces entry)"),
            (format!(r#"{{"mode":"axes","faces":[{f0}],"rotate":1}}"#), "rotate: not valid with mode \"axes\" (put axisU, axisV and scale inside each faces entry)"),
        ];
        for (json, why) in &cases {
            assert_eq!(tex_parse(json).unwrap_err(), *why, "{json}");
        }
        // keepScale / justify are refused too (by the shared checks that run first).
        assert!(tex_parse(&format!(r#"{{"mode":"axes","faces":[{f0}],"keepScale":true}}"#)).is_err());
        assert!(tex_parse(&format!(r#"{{"mode":"axes","faces":[{f0}],"justify":"fit"}}"#)).is_err());
        // The face-count cap.
        let many = (0..=TEX_AXES_MAX_FACES).map(|i| axes_face(i, "")).collect::<Vec<_>>().join(",");
        assert_eq!(
            tex_parse(&format!(r#"{{"mode":"axes","faces":[{many}]}}"#)).unwrap_err(),
            format!("faces: at most {TEX_AXES_MAX_FACES} entries per call")
        );
    }

    #[test]
    fn set_face_texture_axes_writes_each_face_in_one_undo_entry_and_reads_back() {
        let fake = tex_fake();
        let reply = run(
            op_mesh_set_face_texture,
            &fake,
            &tex_params(&format!(r#""mode":"axes","faces":[{},{}]"#, axes_face(2, ""), axes_face(4, ""))),
        );
        assert_eq!(
            reply,
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"mode":"axes","texSet":0,"touched":2,"readBackTruncated":false,"exact":true,"faces":[{"index":2,"axisU":[1,0,0,3],"axisV":[0,-1,0,2],"scale":[0.25,0.5]},{"index":4,"axisU":[1,0,0,5],"axisV":[0,-1,0,2],"scale":[0.25,0.5]}],"undoLabel":"MCP: set face texture"}"#
        );
        let calls = fake.take();
        assert_eq!(calls.iter().filter(|c| c.starts_with("undo_open")).count(), 1);
        assert_eq!(calls.iter().filter(|c| c.starts_with("set_tex")).count(), 2, "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("align") || c.starts_with("justify")), "{calls:?}");
        let undo = calls.iter().position(|c| c.starts_with("undo_open")).unwrap();
        let first_set = calls.iter().position(|c| c.starts_with("set_tex")).unwrap();
        assert!(undo < first_set, "{calls:?}");
        let tex = fake.tex.borrow();
        assert_eq!(tex[2].u, [1.0, 0.0, 0.0, 3.0]);
        assert_eq!(tex[4].scale, [0.25, 0.5]);
        assert_eq!(tex[0], base_tex(0));
    }

    #[test]
    fn set_face_texture_axes_reports_a_write_that_did_not_land_as_inexact() {
        let mut fake = tex_fake();
        fake.drop_set = true;
        let reply = run(
            op_mesh_set_face_texture,
            &fake,
            &tex_params(&format!(r#""mode":"axes","faces":[{}]"#, axes_face(1, ""))),
        );
        assert!(reply.contains(r#""exact":false"#), "{reply}");
        // The read-back is what Hammer holds, not what was asked.
        assert!(reply.contains(r#""axisU":[0,1,0,4]"#), "{reply}");
    }

    #[test]
    fn set_face_texture_axes_refuses_before_touching_the_game() {
        let fake = tex_fake();
        let e = run_err(
            op_mesh_set_face_texture,
            &fake,
            Some(&tex_params(&format!(r#""mode":"axes","faces":[{}]"#, axes_face(6, "")))),
        );
        assert_eq!(e.code, -32602, "{e:?}");
        assert!(e.data.contains("index 6 out of range"), "{e:?}");
        assert!(!fake.take().iter().any(|c| c.starts_with("undo_open")));
        // An unreadable face is refused before the group opens.
        let fake = tex_fake();
        fake.tex_nan.borrow_mut().push(1);
        let e = run_err(
            op_mesh_set_face_texture,
            &fake,
            Some(&tex_params(&format!(r#""mode":"axes","faces":[{}]"#, axes_face(1, "")))),
        );
        assert_eq!(e.name, "TextureSetUnknown");
        assert!(!fake.take().iter().any(|c| c.starts_with("undo_open")));
    }

    #[test]
    fn tex_adjust_rotates_scales_and_shifts() {
        let t = FaceTex { u: [1.0, 0.0, 0.0, 3.0], v: [0.0, 1.0, 0.0, 5.0], scale: [0.5, 0.5] };
        // 90 degrees: exact (no 6e-17 residue), offsets kept.
        let r = tex_adjust(&t, None, None, Some(90.0));
        assert_eq!(r.u, [0.0, 1.0, 0.0, 3.0]);
        assert_eq!(r.v, [-1.0, 0.0, 0.0, 5.0]);
        assert_eq!(r.scale, [0.5, 0.5]);
        let r = tex_adjust(&t, Some([7.0, 8.0]), Some([2.0, 4.0]), None);
        assert_eq!((r.u[3], r.v[3], r.scale), (7.0, 8.0, [2.0, 4.0]));
        assert_eq!(r.u[..3], t.u[..3]);
        // 360 returns the axes; the axes stay unit length at 30 degrees.
        let r = tex_adjust(&t, None, None, Some(360.0));
        for k in 0..3 {
            assert!((r.u[k] - t.u[k]).abs() < 1e-6 && (r.v[k] - t.v[k]).abs() < 1e-6);
        }
        let r = tex_adjust(&t, None, None, Some(30.0));
        let len = (r.u[0] * r.u[0] + r.u[1] * r.u[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_face_texture_world_aligns_the_chosen_faces_in_one_undo_entry() {
        let fake = tex_fake();
        let reply = run(op_mesh_set_face_texture, &fake, &tex_params(r#""mode":"world","faces":[2,4]"#));
        assert_eq!(
            reply,
            r#"{"nodeID":42,"class":"mesh","origin":[1,2.500,-3],"vertexSlots":8,"faceSlots":6,"mode":"world","texSet":0,"touched":2,"readBackTruncated":false,"faces":[{"index":2,"axisU":[1,0,0,0],"axisV":[0,-1,0,0],"scale":[0.25,0.25]},{"index":4,"axisU":[1,0,0,0],"axisV":[0,-1,0,0],"scale":[0.25,0.25]}],"undoLabel":"MCP: set face texture"}"#
        );
        let calls = fake.take();
        // One group, opened after the pre-read, around exactly one align call over two faces.
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("undo_open")).count(),
            1
        );
        assert!(calls.contains(&"align n=2 set=0 mode=1 flags=15".to_string()), "{calls:?}");
        let undo = calls.iter().position(|c| c.starts_with("undo_open")).unwrap();
        let align = calls.iter().position(|c| c.starts_with("align")).unwrap();
        let first_get = calls.iter().position(|c| c.starts_with("get_tex")).unwrap();
        assert!(first_get < undo && undo < align, "{calls:?}");
        // The other faces are untouched.
        let tex = fake.tex.borrow();
        assert_eq!(tex[0], base_tex(0));
        assert_eq!(tex[3], base_tex(3));
        assert_ne!(tex[2], base_tex(2));
    }

    #[test]
    fn set_face_texture_modes_pass_their_numbers() {
        for (body, want) in [
            (r#""mode":"face","keepScale":true"#, "align n=6 set=0 mode=2 flags=7"),
            (r#""mode":"world","keepScale":false,"texSet":1"#, "align n=6 set=1 mode=1 flags=15"),
            (r#""mode":"justify","justify":"fit""#, "justify n=6 set=0 mode=6"),
            (r#""mode":"justify","justify":"right","faces":[5]"#, "justify n=1 set=0 mode=5"),
        ] {
            let fake = tex_fake();
            run(op_mesh_set_face_texture, &fake, &tex_params(body));
            assert!(fake.take().contains(&want.to_string()), "{body}");
        }
    }

    #[test]
    fn set_face_texture_set_mode_applies_per_face_after_the_align() {
        let fake = tex_fake();
        let reply = run(
            op_mesh_set_face_texture,
            &fake,
            &tex_params(r#""mode":"world","faces":[1,3],"shift":[8,9],"scale":[2,2],"rotate":90"#),
        );
        let calls = fake.take();
        // One setter call per face, after the align.
        let sets: Vec<_> = calls.iter().filter(|c| c.starts_with("set_tex")).collect();
        assert_eq!(sets, ["set_tex f=1 set=0", "set_tex f=3 set=0"]);
        let align = calls.iter().position(|c| c.starts_with("align")).unwrap();
        let set0 = calls.iter().position(|c| c.starts_with("set_tex")).unwrap();
        assert!(align < set0);
        // Aligned U=(1,0,0) V=(0,-1,0), rotated 90: U'=(0,-1,0) V'=(-1,0,0); shift/scale absolute.
        assert!(
            reply.contains(r#"{"index":1,"axisU":[0,-1,0,8],"axisV":[-1,0,0,9],"scale":[2,2]}"#),
            "{reply}"
        );
    }

    #[test]
    fn set_face_texture_set_mode_writes_only_the_given_faces() {
        let fake = tex_fake();
        run(op_mesh_set_face_texture, &fake, &tex_params(r#""mode":"set","faces":[5],"shift":[1,2]"#));
        let tex = fake.tex.borrow();
        assert_eq!(tex[5].u[3], 1.0);
        assert_eq!(tex[5].v[3], 2.0);
        assert_eq!(tex[5].scale, [0.5, 0.5]);
        for f in 0..5 {
            assert_eq!(tex[f], base_tex(f));
        }
        assert!(!fake.take().iter().any(|c| c.starts_with("align") || c.starts_with("justify")));
    }

    #[test]
    fn set_face_texture_refuses_before_touching_the_game() {
        let writes = |fake: &Fake| {
            fake.take().iter().any(|c| {
                c.starts_with("undo_open")
                    || c.starts_with("record_node_tree")
                    || c.starts_with("align")
                    || c.starts_with("justify")
                    || c.starts_with("set_tex")
            })
        };
        // Index out of range (the mesh has 6 faces).
        let fake = tex_fake();
        assert_eq!(
            run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(r#""mode":"world","faces":[0,6]"#))),
            OpErr::new(-32602, "InvalidParams", "faces[1]: index 6 out of range (the mesh has 6 faces)")
        );
        assert!(!writes(&fake));
        // Bad mode, justify without a justify member, scale 0, a non-finite number.
        for (body, why) in [
            (r#""mode":"bend""#, "mode: one of world, face, justify, set, axes is required"),
            (r#""mode":"justify""#, "justify: required when mode is \"justify\""),
            (r#""mode":"set","scale":[0,1]"#, "scale: both components must be > 0 (world units per texel)"),
            (r#""mode":"set","shift":[1e60,1]"#, "shift: [u, v] of two finite numbers required"),
        ] {
            let fake = tex_fake();
            let e = run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(body)));
            assert_eq!(e, OpErr::new(-32602, "InvalidParams", why), "{body}");
            assert!(!writes(&fake), "{body}");
        }
        // The node must exist and be a mesh before the params are read.
        let mut fake = tex_fake();
        fake.mesh_class = false;
        assert_eq!(
            run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(r#""mode":"bend""#))),
            OpErr::new(-32602, "InvalidParams", "node is not a mesh")
        );
    }

    #[test]
    fn set_face_texture_nan_read_is_an_error_never_a_success() {
        // Before any write: the set is unreadable on face 2.
        let fake = tex_fake();
        fake.tex_nan.borrow_mut().push(2);
        let e = run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(r#""mode":"world""#)));
        assert_eq!(e.code, -32603);
        assert_eq!(e.name, "TextureSetUnknown");
        assert!(e.data.contains("face 2") && !e.data.contains("maybeExecuted"), "{}", e.data);
        assert!(!fake.take().iter().any(|c| c.starts_with("undo_open") || c.starts_with("align")));
        // After the write (the read-back): maybeExecuted, because the align already ran.
        let mut fake = tex_fake();
        fake.nan_after_write = true;
        let e = run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(r#""mode":"world","faces":[0]"#)));
        assert_eq!(e.name, "TextureSetUnknown");
        assert!(e.data.contains("\"maybeExecuted\":true"), "{}", e.data);
    }

    #[test]
    fn set_face_texture_nan_midway_through_the_per_face_loop_is_maybe_executed() {
        let mut fake = tex_fake();
        fake.nan_after_set = Some(3);
        let e = run_err(
            op_mesh_set_face_texture,
            &fake,
            Some(&tex_params(r#""mode":"set","faces":[1,3,5],"shift":[1,2]"#)),
        );
        assert_eq!(e.code, -32603);
        assert!(e.data.contains("face 3") && e.data.contains("\"maybeExecuted\":true"), "{e:?}");
        // Face 1 was written, face 5 never reached.
        let calls = fake.take();
        assert!(calls.contains(&"set_tex f=1 set=0".to_string()));
        assert!(!calls.contains(&"set_tex f=5 set=0".to_string()), "{calls:?}");
    }

    #[test]
    fn set_face_texture_verifies_what_it_wrote() {
        // The setter silently stores nothing: bit mismatch.
        let mut fake = tex_fake();
        fake.drop_set = true;
        let e = run_err(
            op_mesh_set_face_texture,
            &fake,
            Some(&tex_params(r#""mode":"set","faces":[2],"scale":[2,2]"#)),
        );
        assert_eq!(e.name, "TextureWriteMismatch");
        assert!(e.data.contains("face 2") && e.data.contains("\"maybeExecuted\":true"), "{e:?}");
        // world/face must leave zero offsets (flag 4).
        let mut fake = tex_fake();
        fake.align_offset = true;
        let e = run_err(op_mesh_set_face_texture, &fake, Some(&tex_params(r#""mode":"world","faces":[1]"#)));
        assert_eq!(e.name, "TextureWriteMismatch");
        assert!(e.data.contains("\"maybeExecuted\":true"), "{e:?}");
        // ... unless the caller asked for an explicit shift.
        let mut fake = tex_fake();
        fake.align_offset = true;
        let reply = run(op_mesh_set_face_texture, &fake, &tex_params(r#""mode":"world","faces":[1],"shift":[4,5]"#));
        assert!(reply.contains(r#""axisU":[1,0,0,4]"#), "{reply}");
    }

    #[test]
    fn set_face_texture_checks_every_face_but_caps_only_the_rows() {
        let mut fake = tex_fake();
        fake.tex_big = Some(1200);
        fake.face_slots = 1200;
        *fake.tex.borrow_mut() = (0..1200).map(base_tex).collect();
        // A NaN on a face past the 1000-row cap is still caught after the write.
        fake.nan_after_write = false;
        let ok = run(op_mesh_set_face_texture, &fake, &tex_params(r#""mode":"world""#));
        assert!(ok.contains(r#""touched":1200,"readBackTruncated":true"#), "{ok}");
        assert_eq!(ok.matches(r#""index":"#).count(), 1000);
        let gets = fake.take().iter().filter(|c| c.starts_with("get_tex")).count();
        assert!(gets >= 2400, "pre-read + read-back of every face, got {gets}");
        // The post-write offset check reaches face 1100.
        let mut fake = tex_fake();
        fake.tex_big = Some(1200);
        fake.face_slots = 1200;
        *fake.tex.borrow_mut() = (0..1200).map(base_tex).collect();
        fake.tex_nan.borrow_mut().clear();
        fake.nan_after_set = Some(1100);
        let e = run_err(
            op_mesh_set_face_texture,
            &fake,
            Some(&tex_params(r#""mode":"set","faces":[1100],"shift":[1,1]"#)),
        );
        assert!(e.data.contains("face 1100"), "{e:?}");
    }

    #[test]
    fn set_face_texture_reply_is_capped_but_counts_every_face() {
        let rows: Vec<(usize, FaceTex)> = (0..2).map(|i| (i, base_tex(i))).collect();
        let mut buf = vec![0u8; 4096];
        let mut out = Sb::new(&mut buf);
        let summary = MeshSummary { node_id: 7, origin: [0.0; 3], vertex_slots: 8, face_slots: 6 };
        tex_reply_into(&mut out, &summary, "set", 1, 5, &rows, None);
        let text = String::from_utf8_lossy(out.as_cstr()).into_owned();
        assert!(text.contains(r#""mode":"set","texSet":1,"touched":5,"readBackTruncated":true,"faces":[{"index":0,"axisU":[0,1,0,3],"axisV":[0,0,-1,5],"scale":[0.5,0.5]},{"index":1,"#), "{text}");
        assert!(text.ends_with(r#""undoLabel":"MCP: set face texture"}"#));
    }

    #[test]
    fn set_face_texture_is_registered_and_gated_on_hammer() {
        let row = crate::registry::find("mesh.setFaceTexture").expect("registered");
        assert!(row.implemented && row.blocked_on.is_none());
        assert_eq!(row.needs, crate::registry::NEED_HAMMER);
        assert!(HANDLERS.iter().any(|(name, _)| *name == "mesh.setFaceTexture"));
    }
}
