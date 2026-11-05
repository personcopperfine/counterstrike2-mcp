# Protocol changes in m3.14: `mesh.setFaceTexture` (per-face texture alignment)

Bridge build `m3.14` is m3.13 plus one method (branch `streamline/wp15-align`, WP1.5; ti_convoy
FIXME 29: no UV control). m3.13 (WP1.3) merged after m3.14 was numbered and sits beneath it; an
`m3.14` built from streamline/agent-ux before that merge lacks `cvar.get` and the `-tools` console
gate. Hosts gate on the advertised methods, so this does not matter on the wire.

RE notes: `target/orch/wp15-re.md` (hammer.dll 2000933). Four new pins in
`sigs/signatures/hammer.json` -> `crates/cs2-bridge/src/generated/pins.rs`:
`HFN_MESH_SET_FACE_TEXTURE`, `HFN_MESH_GET_FACE_TEXTURE`, `HFN_MESH_JUSTIFY_FACE_TEXTURE`,
`HFN_MESH_ALIGN_FACE_TEXTURE`. The function types are hand-typed in `ops/hammer.rs` (void returns).

## `mesh.setFaceTexture` (registry: `RUST_METHODS`, `NEED_HAMMER`, queued main-thread write)

Params:

```
{ nodeID, faces?: [idx...],           // get_mesh face order; omitted = every face; no duplicates
  mode: "world" | "face" | "justify" | "set",
  justify?: "left"|"right"|"top"|"bottom"|"center"|"fit"|"fitU"|"fitV",   // required iff mode=justify
  keepScale?: bool,                    // mode world|face only: align flags 7 instead of 15
  shift?: [u, v],                      // texels, absolute (axisU.w, axisV.w)
  scale?: [u, v],                      // world units per texel, absolute, both > 0
  rotate?: deg,                        // added to the current orientation, |deg| <= 36000
  texSet?: 0 | 1 }                     // default 0
```

- `world` / `face`: `CMapMesh::AlignFaceTextures` with an identity origin (the texture grid starts
  at the world origin), flags force|axes|zero-offset|default-scale (15), or without default-scale (7)
  with `keepScale`. `justify`: `CMapMesh::JustifyFaceTextures`, per-face extents (no group bounds).
  `set`: only the optional fields, and it needs at least one of them.
- `shift` / `scale` / `rotate` apply after the mode, per face (each face has its own axes), through
  `CHammerPolyMesh::SetFaceTextureParameters`, inside the same undo entry (`MCP: set face texture`).
- Validation (all before the game is touched, `-32602 InvalidParams {reason}`): mode and justify
  enums, justify only with mode justify, keepScale only with world/face, indices in range and unique,
  every number finite, scale > 0, texSet 0|1, no shift/scale/rotate with mode justify (they would undo it). Then every selected face's texture is read once BEFORE
  the undo group opens; a face the getter cannot read (NaN pre-fill left behind: a texture set the
  mesh lacks, or a stale face) is `-32603 TextureSetUnknown` with nothing written.
- After the write EVERY touched face is read back (only the reply rows are capped): an unreadable one is
  `-32603 TextureSetUnknown` with `maybeExecuted:true`; in the per-face loop the read-back must equal the
  written values bit for bit, and after world/face (without `shift`) both offsets must be 0 (flag 4), else
  `-32603 TextureWriteMismatch` with `maybeExecuted:true`.
- Rotation pivots at texture (0,0) (the axes rotate, the offsets are kept), so the texture also slides.
- Reply (plus the m2.17 write `doc` tail): `{nodeID, class, origin, vertexSlots, faceSlots, mode,
  texSet, touched, readBackTruncated, faces:[{index, axisU:[x,y,z,w], axisV:[x,y,z,w],
  scale:[u,v]}], undoLabel:"MCP: set face texture"}`. The read-back is the first 1000 touched faces
  (`readBackTruncated` says when there are more); floats are shortest round-trip.
- Not done: `mesh.get` does not carry per-face `texture` (the mesh.get reply budget would need the
  per-face bytes; follow-up). Group justify (the 72-byte bounds) is not offered.

## Compatibility

Additive. A bridge older than m3.14 does not advertise the method; hammer-mcp's `align_texture`
answers `NotImplemented{data.missing:["mesh.setFaceTexture"]}` and sends nothing.

## Live checks (not run by the implementer: they need a Hammer session)

See the hammer-mcp `align_texture` section of `hammer-mcp/AGENT.md` and the orchestrator report: box +
`align_texture world` on one face (axes on a world plane, offsets 0, other faces' saved faceData
byte-identical), `undo_last` restores exactly (one entry per call), justify fit on a 128-unit face with a
512 px texture gives scale 0.25, `mode set` round-trips shift/scale/rotate and `rotate 90` matches
Hammer's RotateTexture90CW (fixes the sign), and the ti_convoy case (a face moved off-grid, then `world`:
saved texcoords on the world grid).
