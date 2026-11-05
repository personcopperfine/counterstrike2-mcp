# Protocol changes in m3.7 — lane MESH (the geometry writes)

Bridge build `m3.7` (Rust bridge) = m3.6 + this group.

**Three rows moved from "not ported" to served**: `mesh.create`, `mesh.setGeometry`,
`mesh.setMaterial`. The method table is unchanged (84 registered / 69 implemented), no new error
code, no changed field. `edit.batch`, `entity.tieMeshes` / `entity.untie` and the other `mesh.*`
rows stay `-32011 NotImplemented` (later lanes).

## The wire, row by row

| Row | m2.20 (C bridge) | this build |
|---|---|---|
| `geo_dispatch` write head (`:5612-5659`) | `active_doc` → `doc_editable` → `expect_doc_ok`, in that order, then the op, then the `doc` tail before the overflow check | identical (`ops::hammer::write_gate` + `ops::mesh::geo_dispatch`) |
| `mesh.create` (`:5392-5415`) | parse, root, `MCP: create mesh` group, `CreateMeshElement`, identity, build, insert, record | identical order and strings |
| `mesh.setGeometry` (`:5428-5447`) | `geo_target_mesh` (nodeID form, live node, mesh identity) → parse → `MCP: set mesh geometry` group → `RecordNodeTree` → clear → build | identical |
| `mesh.setMaterial` (`:5449-5474`) | `geo_target_mesh` → `material` member → face count → `CollectHandles` → `MCP: set mesh material` group → `RecordNodeTree` → `AssignMaterialToFaces` | identical |
| result fields | `{"nodeID","class":"mesh","origin","vertexSlots","faceSlots","inputVertices","faces","materials","undoLabel"}` for create; the same summary + `{"inputVertices","faces","undoLabel"}` for setGeometry; the summary + `{"faces","material","undoLabel"}` for setMaterial | byte-identical builders, unit-tested against the C's `sb_add` sequence (`ops::mesh::tests`) |
| error codes | `-32003`, `-32004`, `-32017`, `-32018`, `-32022 NodeNotFound`, `-32602`, `-32603`, `-32603 "response too large (use limit)"` | identical, with the C's `reason` strings and 512-byte `data` object |
| param caps | 65536 vertices / 65536 faces / 1<<20 indices / 64 materials / 260-byte material strings; `faceMaterials` may be `null` or a material string, and the default spelling maps to `-1` | identical |

## What each body does (and where the C is)

* `geom_parse` (`:3188-3271`) is a 1:1 port, including the depth-blind `json_find` lookups, the
  `jstr` reader with only `\"`/`\\`/`\/` escapes (`:3160-3174`), the C's `why` strings, and its
  order: vertices, faces, `material`, `faceMaterials` (a `faceMaterials` that is not an array is
  silently ignored, `:3251`).
* `geo_build` (`:4705-4735`): the rec/point tables the C fills in face order, `MeshInitFromPolygons`
  with `GEO_UV_MODE = 2` and the caller-owned handle vector, then one `AssignMaterialToFaces` per
  distinct material slot (only when the created face count equals the input face count), then the
  "Hammer created no faces (degenerate polygons?)" refusal.
* `geo_reset_mesh` (`:4671-4701`): delete every face through `CollectHandles` + `MeshDeleteFaces`
  (refused above `GEO_MAX_FACES * 4`, and handed an `n`-handle buffer as the C's `geo_alloc(n)`
  is), then the m2.18 clear order — scales `{1,1,1}` (slot 19) only when not already `{1,1,1}`,
  angles `{0,0,0}` (slot 18) only when not already `{0,0,0}` — and always `SetPivotPosition`
  `{0,0,0}` (slot 140).
* `geo_mesh_summary` (`:4736-4746`) reads `node_id_of`, `node_origin`, then the poly-mesh counts;
  the reply builders are pure functions over `MeshSummary`.

## Places the port deliberately differs from the C (no byte on the wire)

1. **The mesh path is split host-side/Windows-side.** The parser, every error mapping and every
   result builder are plain functions; the game access is one `Mesh` trait method per C primitive
   (`ops::mesh::Mesh`, which extends the write lane's `ops::hammer::Write` for `undo_open`,
   `RecordNodeTree` and the `SetAngles`/`SetScales` slots - one implementation, not a copy). The
   host tests drive them through a recording fake, which is how every param error, the call order
   and every builder is asserted byte for byte without a game. The Windows implementation binds
   each new method to the generated vtable slot or the pinned `HFN_*` entry exactly as the C's
   `g_hfn` call does. `mesh_counts` keeps the C's *unguarded* reads of `hm->vertices.count` /
   `hm->faces.count` (`:4674`, `:4742-4743`); `nodeID` / `origin` keep the C's own guarded
   readers.
2. **`geo_reset_mesh`'s `S2` shapes are local structs with const asserts.** `S2HandleVec`,
   `GPolyRec`, `GPolyGroup` and `GVec` (`:3126-3128`, `bridge/s2sdk.h:76`) are declared in the
   Windows module and held to the generated `CUtlVector_CEntityHandle` offsets by `const` asserts,
   the same shape the C asserts with `_Static_assert`.
3. **`geo_alloc` becomes `Vec`.** The C's "out of memory" branches (`geom_parse` `:3189-3194`,
   `geo_build` `:4707-4709`, the `sel` buffer `:4720`) cannot happen with the Rust allocator: a
   failed allocation aborts instead of answering `-32603` / `InvalidParams "out of memory"`. This
   is the only unreachable C answer.
4. **The game-owned vector sizes are clamped to the buffer.** `CollectHandles` /
   `MeshInitFromPolygons` fill a caller-owned vector whose `size` the C trusts; the port clamps
   `size` to `alloc` before indexing, which cannot change a reply for a game that honours its own
   alloc count and turns a game bug into "no faces" instead of an out-of-bounds read. The buffer
   handed to `MeshDeleteFaces` is the C's: `alloc` (`n`, the mesh's face count) zeroed handles with
   the collected ones at the front (`delete_buffer`), so a `CollectHandles` that returns fewer than
   `n` cannot make Hammer write past the block.
5. **The strings Hammer may keep are `'static`.** `CreateMeshElement`'s source file is the C's
   `"mcp_bridge"` literal and every undo label is a C string literal (`undo_open` takes
   `&'static CStr`), as on the C side; a per-call `CString` would be freed while Hammer still held
   the pointer. The `material` and `defmat` pointers are per-call `CString`s on both sides (the C
   passes stack/heap storage that dies with the op).
6. **`is_mesh` reuses `node_class`'s first identity check** (`obj_is(.., HV_MAPMESH_VT)`), so a
   mesh and a non-mesh answer exactly what the C's `is_mesh` answers.

## Python parity coverage (the orchestrator runs these under Wine)

* `bridge/test_geometry.py` — `list_matches_file`, `create_persisted`, `welded`, `pivot_centre`
  (two windings), `moved`, `material`, `undo_material`, `deleted`: covers `mesh.create`,
  `mesh.setGeometry`, `mesh.setMaterial` and their undo entries.
* `bridge/test_geometry_undo.py` — `undo_create`, `undo_set_geometry`, `undo_delete`,
  `face_materials` (create with `faceMaterials` and `null`), `roundtrip_original` (setGeometry on a
  mesh read back from the file).
* `bridge/test_triggers.py` — the `mesh.create` calls at the trigger/brush steps (create before
  tie/untie) and `test_props.py`'s transform/connection cases that create meshes.
* `bridge/test_props.py --static` still greps the C source (it is the m2.18 contract, not this
  lane's). A Rust-side `--static` for this group would need to check, in
  `crates/cs2-bridge/src/ops/mesh.rs`: the three `HANDLERS` names, the five `HFN_*` calls, the
  `GEO_UV_MODE 2` constant, the `MCP: create mesh` / `MCP: set mesh geometry` /
  `MCP: set mesh material` labels, the `-32022` / `-32603` / `-32602` refusals, the conditional
  `host.set_scales` / `host.set_angles` and the always-`set_pivot` in `geo_reset_mesh` (the slots
  themselves are `ops::hammer`'s `Write` impl), and that `geo_reset_mesh` never writes the origin.
