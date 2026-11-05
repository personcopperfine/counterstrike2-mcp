# WP1.5 RE: per-face texture (UV) control on Hammer meshes (ti_convoy FIXME 29)

hammer.dll build 2000933, IDB `target/ida/win64-2000933/hammer.dll.i64`. Addresses use the 0x180000000 base.
Tags: [V] read in the decompile or disassembly of this build, [I] inferred, [L] needs a live check.
Pinned in `sigs/signatures/hammer.json` (+ `sigs/fingerprints/hammer-fp.json`), regenerated into
`bridge/bridge_pins.h` and `crates/cs2-bridge/src/generated/pins.rs`:

| Pin | RVA | Function |
|---|---|---|
| `HFN_MESH_SET_FACE_TEXTURE` | 0x8E6C80 | CHammerPolyMesh::SetFaceTextureParameters |
| `HFN_MESH_GET_FACE_TEXTURE` | 0x8D9B10 | CHammerPolyMesh face texture getter |
| `HFN_MESH_JUSTIFY_FACE_TEXTURE` | 0x9B5520 | CMapMesh::JustifyFaceTextures (wrapper over JustifyFaceTextureParmeters 0x1808DFEE0) |
| `HFN_MESH_ALIGN_FACE_TEXTURE` | 0x9B4B70 | CMapMesh::AlignFaceTextures (wrapper over InitializeFaceTextureParameters 0x1808DE060 + CInitializeUsingComputeTextureAxes) |

`sigcheck.py validate`: 0 problems. `sigcheck.py resolve --strict`: rc 0, hammer.dll 872 ok / 0 stale.
I checked each sig for uniqueness with IDA find_bytes (1 match each) and sigcheck resolved it to the same RVA.

## Data model [V]

The per-face texture state lives in the CHammerPolyMesh **faceData** streams. Each texture-coordinate
set `s` has three streams, and `sub_180CC7D60(s)` returns their ids at [1], [2] and [3]:
- `textureAxisU`: Vector4. xyz is the axis, w is the offset in texels.
- `textureAxisV`: Vector4.
- `textureScale`: Vector2, in world units per texel.

The face's faceData index comes from `sub_180CC10D0(poly, &faceHandle)` (GetFaceDataIndex). These are
the .vmap `faceData` streams `textureAxisU:0/V:0/textureScale:0`. Set 1 holds `...:1`.

Texel coordinate of a world point P on a face (`sub_180AE4480`, used by Justify):
`s = dot(P, axisU.xyz) / scaleU`, `t = dot(P, axisV.xyz) / scaleV`, then `+ axis.w`.
Justify-left writes `axisU.w = -min s`. Justify-center writes `axisU.w = texW/2 - (min s + max s)/2`.
So w is in texels and UV = texel / texture size. [V for the formula, I for the final divide]

texSet: Hammer passes `sub_180847890(byte_1824C94B0)`, which is `*(int*)(settings+116)`, the editor's
active UV set. 0 and 1 are single sets. 3 means "every set the mesh has" (`sub_180CCAE30`). A set the
mesh doesn't have is skipped silently (`sub_180CCC240`). The bridge should pass 0 explicitly.

Every setter below opens its own CHammerPolyMesh edit scope (`sub_180C89910(scope, "<name>")` +
`sub_180C96DA0` ... `sub_180CAA280`). It then recomputes the touched faces' texcoords with
`sub_180CCFAA0(poly, faces.mem, faces.size, sub_180CC7EE0(set))`. Undo is the caller's job.

Face vector argument: `int *a2` with the size at `[0]` and `MeshHandle*` (16 B each) at `+8`. That is
the bridge's existing `HandleVec {size, pad, mem, alloc, flags}`, the same shape it already passes to
`HFN_MESH_DELETE_FACES`.

## Q1: set a face's texture parameters by handle [V]

`HFN_MESH_SET_FACE_TEXTURE` = `sub_1808E6C80(poly, faces*, texSet, const Vector4 *axisU, const Vector4 *axisV, const Vector2 *scale)`:
```
sub_180C89910(v45, "SetFaceTextureParameters", 0, 1); sub_180C96DA0(poly, v45);
... per set: for each face: v28 = sub_180CC10D0(poly, &face);
    *(_OWORD *)(axisUstream + 16*v28) = *axisU;   // axisU incl. w offset
    *(_OWORD *)(axisVstream + 16*v28) = *axisV;
    scale stream[v28] = {scale.x, scale.y};
sub_180CCFAA0(poly, faces.mem, faces.size, sub_180CC7EE0(set));  sub_180CAA280(poly, v45);
```
- The CMapMesh thunk is `0x1809B8790` (`add rcx, 498h; jmp sub_1808E6C80`). The bridge calls the core
  with `poly(node)` (`HOFF_MESH_POLYMESH`, the same as `assign_material`).
- Hammer's own callers:
  - The texture-properties panel `sub_180256EB0` reads each face's record `{axisU, axisV, ?, scale}`,
    lets the panel callback edit it, then calls `RecordNodeTree(hist, mesh+200, 1)` + thunk(mesh, {face}, activeSet, &U, &V, &scale).
  - `sub_180A66F90` (a view-projected apply).
  - `sub_180A6D320` (lift/apply texture from another face).
- So Hammer itself changes shift, scale and rotation by rewriting the absolute axes through this one
  setter. There's no separate shift or rotate-by-angle API.
- Other ops seen. I pinned none of them because the setter covers them:
  - `ScaleFaceTextureParmeters 0x1808E5CE0(poly, faces, set, const float mul[2])` multiplies the
    scale. Its CMapMesh thunk is 0x1809B7890.
  - `RotateFaceTextureParmeters 0x1808E5900(poly, faces, set)` is a fixed 90-degree step
    (`sub_180AE4B00`). Thunk 0x1809B7880, used by RotateTexture90CW `sub_180383680`.
  - Nudge (`sub_180385710`, "Nudge Texture %s %d") goes through `sub_180A6CE70` →
    `CMapMesh 0x1809B4B00` → InitializeFaceTextureParameters with `CShiftTexturesByWorldDistanceAlgorithm`
    (a shift by the grid size in world units).

## Q2: world-align and justify [V]

Face-selection command table `sub_180377B60` (this build, same address as the old notes):
- `JustifyTop/Bottom/Left/Center/Right/Fit` → `sub_1803828F0(tool, mode)` with mode 1/2/3/4/5/6.
- `AlignWorld/AlignFace` → `sub_180382CA0(tool, 1|2)`.
- `NudgeTextureRight/Left/Up/Down` → `sub_180385710(0..3)`.

The selection stubs only split the selection per mesh (`sub_18091ECF0(sel, &perMeshFaceVecs, &meshes)`)
and then call a **by-face-vector, one-mesh** CMapMesh function. So the bridge doesn't need the selection
at all. It calls the same CMapMesh function on one mesh with its own handle vector, exactly like the
by-index operators.

Justify, `sub_180A6A730(doc, sel, mode, treatAsOne)` (the "Justify texture" op is opened by `sub_1803828F0`):
```
sub_18091ECF0(sel, &perMesh, &meshes);
if (treatAsOne) { bounds = {+FLT_MAX x3, -FLT_MAX x3, ...}; for each mesh: sub_1808EA010(poly, faces.mem, faces.size, worldMtx, &bounds); }
for each mesh: sub_18086DD30(hist, mesh+200, 1);                      // RecordNodeTree
               sub_1809B5520(mesh, &perMesh[i], activeSet, mode, treatAsOne ? &bounds : 0);
```

`HFN_MESH_JUSTIFY_FACE_TEXTURE` = `sub_1809B5520(mesh, faces*, set, mode, bounds|0)`. It gets the world
matrix via vfn+0xA0 (`*a1 + 160`) and calls `sub_1808DFEE0(mesh+0x498, faces, set, mode, &worldMtx, bounds)`.
- When bounds is 0, it computes each face's own extent: `sub_1808EA010(poly, &face, 1, worldMtx, &local)`.
- Then `sub_180AE48C0(mode, bounds, texSize, &U, &V, &scale)`:
  - mode 0: offsets = 0.
  - mode 1: `V.w = -minT` (top).
  - mode 2: `V.w = texH - maxT` (bottom).
  - mode 3: `U.w = -minS` (left).
  - mode 4: center both.
  - mode 5: `U.w = texW - maxS` (right).
  - mode 6: fit, which sets the scale to 1, then scale *= extent/texSize, then justifies top + left.
  - modes 7 and 8: fit U only / fit V only.
  - A zero scale is replaced first with the default texture scale (`sub_18097C340(off_1821DFDB8)`).
- texSize comes from the face's material (`sub_1808C6220`, per material index via the face material
  stream).

AlignWorld/AlignFace, `sub_180A671F0(doc, sel, alignMode, const float *origin, set)`, called from
`sub_180382CA0` after `AddOperation("Align Texture: World|Face")`:
```
for each mesh: sub_18086DD30(hist, mesh+200, 1);
               sub_1809B4B70(mesh, &perMesh[i], set, alignMode, 15, origin);
```

`HFN_MESH_ALIGN_FACE_TEXTURE` = `sub_1809B4B70(mesh, faces*, set, alignMode, flags, const float origin[12])`
→ `sub_1808DDFF0(poly, faces, set, worldMtx, origin, alignMode, flags)`. That function builds a
`CInitializeUsingComputeTextureAxes {vtbl, mode, flags, origin 3x4 copied (48 B)}` and calls
`InitializeFaceTextureParameters sub_1808DE060`.

Per face, the algorithm (`sub_1808C8590` → `sub_180AE3C20(U, V, scale, normal, mode, dominantAxis, flags)`) does this:
- flags & 1, force. Without it, a face whose axes are already non-zero is **skipped**.
- flags & 4: `U.w = V.w = 0`.
- flags & 8: scale = the default texture scale (both).
- flags & 2: new axes.
  - mode 1 (world): U/V come from the static tables `qword_1821E1900` / `qword_1821E1950`, indexed by
    the normal's dominant axis (`sub_180AE41E0`).
  - mode 2 (face): the axes are built in the face plane with cross products.
  - Then `U.w = (origin[3] mod texW) / scaleU` and `V.w = -(origin[7] mod texH) / scaleV`.
- Hammer passes flags 15. Its `origin` is 64 bytes copied from `*(*(tool+240)+312)+3596`. I haven't
  identified that object [I: a texture-lock or work-plane transform].
- The game's own nudge path passes the identity constant `xmmword_181DCDCF0` (= 1,0,0,0 / 0,1,0,0 /
  0,0,1,0) as that argument. An identity origin gives offset 0: the texture grid starts at the world
  origin, which is what ti_convoy needed.

Answer to Q2: both can be driven by a face-handle vector on one mesh, without the selection. That makes
the m3.12 `selection.set` route unnecessary. (If someone wants it anyway: `selection.set {faces}` then
invoke the command. But no command-dispatch pin exists, and the by-vector route is simpler and exact.)

## Q3: read-back [V]

`HFN_MESH_GET_FACE_TEXTURE` = `sub_1808D9B10(poly, const MeshHandle *face, set, Vector4 *outU, Vector4 *outV, Vector2 *outScale)`:
```
if (!sub_180CCC240(poly, set)) return;          // set absent: outputs untouched
streams via sub_180CC7D60(set) ...
idx = sub_180CC10D0(poly, face); if (idx >= 0) { *outU = U[idx]; *outV = V[idx]; *outScale = S[idx]; }
```
- It's a pure read. There's no return value to test, so pre-fill the outputs with NaN and treat a NaN
  left behind as "not read".
- Callers: `sub_180925430` (a face-ref wrapper, used by the lift/apply op) and the texture panel
  `sub_180256EB0` (re-reads after apply). Set 3 is not valid here: pass 0 or 1.

## Q4: pinned entries

| Pin | Sig (start) | Uniqueness | Calling convention (win64 fastcall, all return void) | Tag |
|---|---|---|---|---|
| SET 0x8E6C80 | `4C 89 4C 24 20 44 89 44 24 18 48 89 54 24 10 ...` + ref "SetFaceTextureParameters" at +61 | 1 match | (poly, faces*, u32 set, const Vec4*, const Vec4*, const Vec2*) | V |
| GET 0x8D9B10 | `48 89 5C 24 18 56 41 56 41 57 48 83 EC 30 4C 8B F2 ...` | 1 match | (poly, const MeshHandle*, u32 set, Vec4*, Vec4*, Vec2*) | V |
| JUSTIFY 0x9B5520 | full wrapper body through `E8 ?? ?? ?? ?? 48 8B 5C 24 70` | 1 match | (CMapMesh*, faces*, u32 set, u32 mode, const void* bounds = NULL) | V |
| ALIGN 0x9B4B70 | full wrapper body through `E8 ?? ?? ?? ?? 4C 8D 5C 24 70` | 1 match | (CMapMesh*, faces*, u32 set, i32 mode, i32 flags, const float origin[12]) | V |

Fingerprints: I generated them with `sigs/ida/ida_fp.py`'s own `fingerprint()` and `_rivals()` (487 rivals)
inside the 2000933 IDB, and appended them to `hammer-fp.json`. None of them is weak.
- Caveat: `fingerprints/BUILD` says 2000930 and these four were taken on 2000933.
- The entries carry only `expect["2000933"]`, because I did not check them against 2000930.
- Fixing that is a re-run of `ida_fp.py` / `sigupdate.py record`.

No struct offset or vtable slot is needed beyond what the bridge already has: `HOFF_MESH_POLYMESH`
(+0x498), the record-node path (+0xC8 is inside `record_node_tree`), and the HandleVec shape.

# Bridge method spec

## `mesh.setFaceTexture` (new, write; main thread; `geo_dispatch`, `expectDoc`)

Params:
```
{ nodeID, faces?: [faceIndex...]   // get_mesh face order; omitted = all faces
  mode: "world" | "face" | "justify" | "set",
  justify?: "left"|"right"|"top"|"bottom"|"center"|"fit"|"fitU"|"fitV"   // mode=justify
  keepScale?: bool                  // mode=world|face: flags 7 instead of 15 (the scale stays)
  shift?: [u, v]                    // texels; absolute, written to axisU.w / axisV.w
  scale?: [u, v]                    // world units per texel; absolute, both > 0
  rotate?: deg                      // added to the current orientation (see below)
  texSet?: 0|1 (default 0) }
```
`mode=set` applies only the optional shift/scale/rotate. With the other modes, those fields are applied
**after** the align/justify, inside the same undo entry.

Body:
1. `geo_target_mesh(host, doc, params)` → mesh. Validate the params.
   - `faces` indices: 0 <= i < face count, no duplicates. Otherwise -32602.
   - `scale` must be finite and > 0. Every float must be finite.
2. `collect_faces(mesh, n)` (`HFN_COLLECT_HANDLES`). Build a `HandleVec` holding the selected handles
   in request order (sized buffer, `UTLVEC_EXTERNAL`, like `delete_faces`).
3. `hist = undo_open(doc, c"MCP: set face texture")`, then `record_node_tree(hist, mesh)`. That is the
   same pair as setMaterial, and Hammer's commands do the same per mesh.
4. Run the mode:
   - world/face: `HFN_MESH_ALIGN_FACE_TEXTURE(mesh, &vec, texSet, 1|2, keepScale ? 7 : 15, IDENTITY_3x4)`.
     IDENTITY_3x4 is a bridge-side `[f32; 12]` (1,0,0,0, 0,1,0,0, 0,0,1,0). It's data, not an address.
   - justify: `HFN_MESH_JUSTIFY_FACE_TEXTURE(mesh, &vec, texSet, m, NULL)`. m = top 1, bottom 2, left 3,
     center 4, right 5, fit 6, fitU 7, fitV 8.
5. If shift/scale/rotate are given, then **per face** (each face has its own axes, and SET writes one
   triple to every face in the vector):
   1. `GET(poly, &h, texSet, &U, &V, &S)` with the outputs pre-filled with NaN. A NaN left behind means
      the face or set couldn't be read: fail -32603 (the undo entry stays open-but-harmless; see the
      failure modes).
   2. rotate θ: `U' = cosθ·U.xyz + sinθ·V.xyz` and `V' = -sinθ·U.xyz + cosθ·V.xyz`. Offsets are kept.
      The sign convention is [L]: match Hammer's RotateTexture90CW on a test face.
   3. scale: `S = scale`. shift: `U.w = shift[0]`, `V.w = shift[1]`.
   4. `SET(poly, &{h}, texSet, &U', &V', &S)`, with a one-handle vector.
6. Close the operation (the geo_handler path, as setMaterial does). Reply with: the mesh summary, the
   face count touched, and the per-face read-back (GET after the op), e.g.
   `faces:[{index, axisU:[x,y,z,w], axisV:[...], scale:[u,v]}]`.

## Read-back in `mesh.get` (optional, pure read)

Add a `texture` member to each face: `{axisU:[4], axisV:[4], scale:[2]}` for set 0, from `HFN_MESH_GET_FACE_TEXTURE`
(NaN pre-fill; when the read fails, leave it out rather than guess). In hammer-mcp, `get_mesh` passes it
through. The MCP tool is `set_mesh_face_texture` (or an extension of `set_mesh_material`) in
`hammer-mcp/src/tools/mesh_live.rs`, using the same face-index conventions as `get_mesh` and
`delete_mesh_faces`.

## Failure modes

- Stale indices if the mesh changed after the caller's `get_mesh`. Same mitigation as the other
  by-index ops: reply with the fresh read-back, and offer an optional `expectFaceCount` guard.
- A texSet the mesh lacks: SET, ALIGN and JUSTIFY silently do nothing, and GET leaves the outputs as
  NaN. Detect it through the read-back and report it.
- `world`/`face` reset the scale to the editor's default texture scale, a global setting (flags 8). Use
  `keepScale` or an explicit `scale` to control that.
- justify fit and the right/bottom/center modes depend on the material's texture size (`sub_1808C6220`).
  A material whose texture isn't loaded may give odd sizes [L].
- Without flag 1 (force), align skips faces that already have axes. Always pass 1.
- Group justify ("treat as one") is not offered. It needs the 72-byte world extreme-point bounds from
  `sub_1808EA010`, which is not pinned. That's a follow-up if wanted.
- Threading: main thread only, like every geo op.
- The render/DME refresh after these CHammerPolyMesh-level edits is assumed to happen the same way as
  for `AssignMaterialToFaces` (core call, then the bridge's normal close path) [I/L].

## What a live test must check

1. Create a box. `get_mesh` captures the per-face texture. Then `setFaceTexture {mode:"world", faces:[k]}`.
   The read-back must show axes on a world plane and `U.w = V.w = 0`.
2. `save_map`, then read the .vmap. Face k's `textureAxisU:0/V:0/textureScale:0` must change to the
   read-back values, and the faceData of every other face must stay byte-identical to before.
3. `undo_last` restores face k's axes, offset and scale exactly (re-read with mesh.get, and after a
   save if you like). There must be one undo entry per call.
4. Justify left/top/fit on a 128-unit face. With fit on a 512 px texture, the scale must be
   0.25 = 128/512. Left must leave `U.w = -min s`.
5. shift/scale/rotate (`mode:"set"`):
   - the values round-trip through the read-back;
   - rotate 90 matches the axes RotateTexture90CW gives (this fixes the sign);
   - the UVs in the saved .vmap `texcoord:0` move accordingly.
6. The ti_convoy case: a 128-unit face moved off-grid, then `world` (identity origin). Its saved
   texcoords must be multiples of 128/texW at the world-grid corners.
