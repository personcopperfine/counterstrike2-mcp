#!/usr/bin/env python3
"""sdkcheck.py - the generated s2sdk numbers vs the Windows game binaries (V1a).

Compares what s2sdk generates - the dual-ABI IR in crates/s2sdk/s2sdk-sys/ir/, visible
to the bridge as S2_VTLEN_*/S2_SLOT_* in bridge/s2sdk_gen.h (design 2.3, WP C1) -
with the game binaries under $CS2_BINS, through sigcheck.py's Resolver and PE
reader (imported, never duplicated: one binary-format implementation, exactly
like bridge/pins.py).

Three check groups (design "3. Verification" item 3):

(a) vtable lengths: for every generated vtable whose game implementation the
    sigs database has located as a kind=vtable entry (the VT_PAIRS table below
    names the pairs and the rule for each), read the located table's RTTI COL
    (assert the COL's class name is the class the entry claims and the COL
    offset the locator says), measure the vftable length with
    sigcheck.vtable_len - the same scan S1b's vtlen locator uses - and compare
    with the IR's msvc vtable length. The rule per entry: the game's concrete
    vtable can legitimately be LONGER than the upstream interface (a derived
    class adds virtuals), so the default comparison is `measured >= upstream`;
    `==` is used only where the sigs entry pins the interface's exact
    implementation sub-table in the binary AND the database carries the
    measured length as a vtlen const - the ISource2ServerConfig view in
    server.dll, whose length is GSVC_GAMECONFIG_VT_SLOTS; the bridge sizes its
    vftable copy by S2_VTLEN_ISource2ServerConfig, which is right only because this
    row holds the two `==` (F3).
    Any extra slot there means the upstream header is stale against the game
    (upstream PR path, workflows/game-update.md lane A). The rule `==+N` states that
    the concrete class adds exactly N virtuals of its own (CSoundOpSystem: +2).
(b) `sdk` entries (check_sdk): every sigs entry carrying an `sdk` field - the generated
    expression in tie syntax (S2_SLOT_X, S2_SLOT_X*8, S2_OFF_X+8, S2_OFF_A+S2_OFF_B) the
    bridge compiles instead of a pin - is evaluated against bridge/s2sdk_gen.h and must
    equal the entry's own measurement (a vslot locator's slot, else the resolved value).
    Row tag `sdk:<module>/<entry>`: PASS, or SKIP (LITERAL) for an entry with no locator.
(c) consts: every S2_CONST_* that has a game-measured counterpart among S1b's
    *located* sigs consts (locator present; literal `value` entries are not
    game measurements) must agree exactly. GSVC_GAMECONFIG_VT_SLOTS is that
    counterpart of S2_VTLEN_ISource2ServerConfig and is checked by (a)'s `==`
    row, so it is not repeated here.

Windows-only groups (allowlist.toml `platforms = ["windows"]`, IR `"platforms":
["windows"]`): the tool modules (toolframework2, hammer, pet, modeldoc_editor) have no
Linux build, so no Itanium number for them is a claim about any binary. `--platform
linux` therefore never checks them: each such group gets one `group:<name>` SKIP row
and each of its vtable classes / slot pins a SKIP row naming the reason, never a
PASS (nothing measured) and never a FAIL (nothing is wrong). The windows lane treats
them like any other group.

A concrete-class vtable-length row needs no new table shape: it is a VT_PAIRS row
whose sigs entry is a `kind=vtable` + `rtti` locator for the implementing class
(the class's RTTI COL is read back and must name it), compared with the IR's msvc
vtable length under rule `==` (rule `>=` when the concrete class may add virtuals).
The `==` rule is only sound where the implementing class adds none beyond the
interface - the RE spec says so for it; --self-test exercises a `==` row on both
sides (a matching IR passes, one slot off fails).

Output: one line per fact `PASS|FAIL|SKIP <what> upstream=<n> game=<n> <why>`,
a coverage summary, exit status 0 iff no FAIL. Every generated vtable class
gets a line, PASS/FAIL when checked, SKIP with the reason when no sigs entry
locates the class's implementation (or upstream emits no vftable for it).

--self-test perturbs in-memory copies of IR values (an msvc vtable length, an
msvc_slot, a measured const) and asserts that each doctored copy makes its
check FAIL. Nothing on disk is touched, no binary is modified. The linux half
is exercised the same way on an in-memory IR copy (a synthetic stale-upstream
vtable for a DB-measured class) and on an in-memory copy of the linux DB (a
located entry whose locator stops matching - what a game change does).

--platform windows|linux: the pair/const tables are keyed by platform. `linux`
checks the Linux half the same way - the ELF modules under --tree (default
target/game-bins-linux-2000930), a *different* game build from $CS2_BINS, through the
same sigcheck Resolver (its ELF reader, relocation-aware reads included, is the
shared binary-format implementation - never duplicated):

(a) vtable lengths: every DB-measured Itanium VTLEN - an `itanium_vtable`
    locator with `measure` (libtier0's VTLEN_IKeyValuesDumpContextAsText /
    VTLEN_CKeyValuesDumpContextAsDevMsg) - compares with gen.py's
    `itanium_vtable_len` under rule `==`: the locator is the class's own exact
    `_ZTV` symbol and `measure: "symbol"` reads the linker's `st_size` of that
    table group (header + every slot, no scan under-count), so measured ==
    upstream's Itanium length is the claim. An upstream class that has a
    measured VTLEN but no vtable in the IR (the keyvalues dump contexts - not
    in gen.py's allowlist) SKIPs: gen.py derives an Itanium length only from
    IR vtables and adding the class would change the generated IR (out of
    scope for a check script). Every IR vtable class itself also gets a line:
    an evidence probe over the staged modules' symbol tables says whether a
    `_ZTV` symbol exists for it at all (none does - the game's C++ symbols
    are hidden; linuxvtables.py prints that survey) and the row SKIPs saying
    so.
(i) entity identity geometry (linux only): libclient.so's CG_ENTITY_CHUNKS
    (the chunk-pointer array) and CCONST_IDENTITY_CHUNK_BITS (the lookup's
    shift immediate) resolve and are shown; the identity stride the lookup
    multiplies by (`imul 0x70` = 112) comes from libclient.json
    CCONST_IDENTITY_STRIDE - the same lookup, its sig anchored on the imul and
    a `disp` step reading the immediate, exactly as CCONST_IDENTITY_CHUNK_BITS
    reads the shr - and compares `==` with the IR's Itanium
    sizeof(CEntityIdentity), the number S2_SIZEOF_CEntityIdentity pins on the
    MSVC side. Without a *STRIDE* const/offset entry in the DB the row falls
    back to a SKIP naming the entry it expects - this script adds no DB entries.
(c) consts with a Linux-measured counterpart: none exist yet; every located
    linux const without a generated counterpart comes out SKIP with its
    resolved value, and a resolution failure is FAIL (fail closed).
"""

import argparse
import copy
import json
import os
import sys
from pathlib import Path

SIM: Path = Path(__file__).resolve().parent            # sigs/scripts
SIGS = SIM.parent                                      # sigs
REPO = SIGS.parent                                     # cs2-mcp
sys.path.insert(0, str(SIM))
import sigcheck as sc                                  # noqa: E402  (the shared resolver/PE reader)

GEN_DIR = REPO / "crates" / "s2sdk" / "s2sdk-sys" / "gen"
IR_DIR = REPO / "crates" / "s2sdk" / "s2sdk-sys" / "ir"
sys.path.insert(0, str(GEN_DIR))
import gen                                             # noqa: E402  (itanium_vtable_len)


# ---------------------------------------------------------------------------
# the pair tables (documented knowledge; the check is generic, the pairs are not)

# (platform, ir group, upstream class, module, sigs entry, rule, why).
# rule ">=": the locator names a concrete class implementing the upstream
#   interface in its primary chain; the game class may add virtuals, so only
#   `measured >= upstream` is provable from the length scan.
# rule "==+N": the concrete class adds exactly N virtuals of its own to the interface's table
#   (measured == upstream + N); the `why` names them. Like "==" it is a claim that the
#   binary has no other entry, so an added or removed interface slot FAILs it.
# rule "==": the sigs entry IS the interface's exact implementation sub-table
#   (the RTTI complete-object locator sits at a secondary-base col_offset) and
#   the DB carries the measured length as a vtlen const - here
#   GSVC_GAMECONFIG_VT_SLOTS, which the bridge's vftable copy spells as
#   S2_VTLEN_ISource2ServerConfig. Equality is real, not an accident of
#   today's numbers: an added virtual moves the table tail the locator's pin
#   proof relies on.
PLATFORM = "windows"
VT_PAIRS = [
    (PLATFORM, "tier0", "ICvar", "tier0.dll", "T0V_CCVAR_VT", ">=",
     "CCvar implements ICvar as its primary chain (RTTI col 0); a concrete class may add virtuals, so the surface stays a lower bound"),
    (PLATFORM, "tier0", "IAppSystem", "tier0.dll", "T0V_CCVAR_VT", ">=",
     "derived-chain row: IAppSystem's 11 slots sit at the front of the same CCvar table (CCvar : ICvar : IAppSystem)"),
    (PLATFORM, "tier0", "IBaseInterface", "tier0.dll", "T0V_CCVAR_VT", ">=",
     "derived-chain row: IBaseInterface's CreateInterface slot is the front of the same CCvar table"),
    (PLATFORM, "schema", "ISchemaSystem", "schemasystem.dll", "SSV_SCHEMASYSTEM_VT", ">=",
     "the pinned ISchemaSystem view of CSchemaSystem (RTTI col 0); CSchemaSystem implements the upstream interface"),
    (PLATFORM, "schema", "ISchemaSystemTypeScope", "schemasystem.dll", "SSV_TYPESCOPE_VT", ">=",
     "the pinned type-scope view of CSchemaSystemTypeScope (RTTI col 0); CSchemaSystemTypeScope implements ISchemaSystemTypeScope"),
    (PLATFORM, "entity", "CEntityInstance", "server.dll", "CBaseEntity", ">=",
     "CBaseEntity derives CEntityInstance (its longest chain head is the first base), so its table contains at least the upstream slots"),
    (PLATFORM, "entity", "CEntityInstance", "client.dll", "C_BaseEntity", ">=",
     "same fact on the client side: C_BaseEntity derives CBaseEntity -> CEntityInstance"),
    (PLATFORM, "engine", "ISource2ServerConfig", "server.dll", "GSV_GAMECONFIG_SERVER_VT", "==",
     "exact-implementation row: the locator pins the ISource2ServerConfig view of CCSGameConfiguration (RTTI col 8, object+8) and the DB measures its length as GSVC_GAMECONFIG_VT_SLOTS (the bridge's copy is sized by S2_VTLEN_ISource2ServerConfig, F3)"),
    (PLATFORM, "engine", "ISource2Server", "server.dll", "CSource2Server", "==",
     "exact-implementation row: CSource2Server (RTTI col 0) is the Source2Server001 object's ISource2Server table and the DB measures its length as CSource2Server_VT_SLOTS (103: no destructor, no virtual of its own in the primary table; the secondary base has its own COL)"),
    (PLATFORM, "engine", "ISource2ServerConfig", "client.dll", "CCSGameConfiguration::col_8", ">=",
     "the same interface-view shape (RTTI col 8) in client.dll; the DB carries no vtlen const for it, so only >= is claimed"),
    (PLATFORM, "tier0", "ITestScriptMgr", "tier0.dll", "T0V_TESTSCRIPTMGR_VT", "==",
     "CTestScriptMgr (RTTI col 0, the TestScriptMgr001 singleton) implements the interface and adds no virtual: its single table has 18 entries, the RE spec read each body"),
    (PLATFORM, "toolframework2", "IToolFramework2", "toolframework2.dll", "TFV_FRAMEWORK_VT", "==",
     "CToolFramework2 (RTTI col 0, the only table) implements the interface and adds no virtual: its 98 entries were matched slot by slot by the RE spec"),
    (PLATFORM, "toolframework2", "IToolSystem2", "pet.dll", "CBaseToolSystem2", "==",
     "CBaseToolSystem2 (RTTI col 0) is the IToolSystem2 implementation base and adds no virtual: its table has exactly 17 entries (ActivateTool is a _purecall there), per RE2 3.2"),
    (PLATFORM, "toolframework2", "IToolFrameListener", "toolframework2.dll", "IToolFrameListener", "==",
     "CQInputBindManager's IToolFrameListener view (RTTI col 16, object+16): a 3-entry table that ends where the next table's locator begins, per RE2 3.3"),
    (PLATFORM, "soundsystem", "ISoundOpSystem", "soundsystem.dll", "SNV_SOUNDOPSYSTEM_VT", "==+2",
     "CSoundOpSystem (RTTI col 0, the SoundOpSystem001 singleton) is the 101-slot interface (11 IAppSystem + 90 own) plus its own two LoadManifestStacksForParsing overloads, 103 entries; MSVC gives its overrides of the secondary base ISoundEventManager no slot of their own (RE4 section 0)"),
    (PLATFORM, "soundsystem", "ISoundEventManager", "soundsystem.dll", "SNV_SOUNDEVENTMGR_VT", "==",
     "CSoundOpSystem's ISoundEventManager subobject (RTTI col 8, object+8): a 63-entry table that ends where the next COL begins, adding no virtual (RE4 section 0)"),
    (PLATFORM, "engine", "IEngineService", "engine2.dll", "IEngineService", "==",
     "IEngineService's own table (RTTI col 0): 11 IAppSystem slots, its 11 virtuals and the destructor it declares last - 23 entries; the CBaseEngineService and concrete service tables inherit that destructor slot (RE5 section 0)"),
    (PLATFORM, "engine", "IRenderService", "engine2.dll", "EV_RENDERSERVICE_VT", "==",
     "CRenderService (RTTI col 0, the RenderService_001 singleton) implements the interface and adds no virtual: 39 entries, the 23 of IEngineService + 16 (RE5 section 4)"),
    (PLATFORM, "engine", "IInputService", "engine2.dll", "EV_INPUTSERVICE_VT", "==",
     "CInputService (RTTI col 0, the InputService_001 singleton) implements the interface and adds no virtual: 63 entries, the 23 of IEngineService + 40 (RE5 section 4)"),
    (PLATFORM, "engine", "IVEngineClient2", "engine2.dll", "EV_ENGINECLIENT_VT", "==",
     "CEngineClient (RTTI col 0, the Source2EngineToClient001 singleton) implements the interface as its single base and adds no virtual: 185 entries, the 18 of ISource2Engine + 167 (RE5 section 4)"),
    (PLATFORM, "rendersystem", "IRenderDevice", "rendersystemvulkan.dll", "RV_RENDERDEVICE_VT", "==+37",
     "CRenderDeviceVulkan (RTTI col 0, the RenderDevice003 singleton) is the 255-slot interface (IRenderDevice declares no destructor) plus 37 entries of its own: CRenderDeviceBase's scalar deleting destructor and 35 virtuals, then 1 of CRenderDeviceVulkan's own, 292 entries (RE6 section 4)"),
    (PLATFORM, "particles", "IParticleSystemMgr", "particles.dll", "CParticleSystemMgr", "==",
     "CParticleSystemMgr (RTTI col 0, the ParticleSystemMgr003 singleton) implements the interface and adds no virtual: 61 entries, the 11 of IAppSystem + 50 (MSVC gives its overrides of the two secondary bases no slot of their own), RE7 section 4"),
    (PLATFORM, "particles", "IParticleFunctionDefinition", "particles.dll", "CParticleFunctionDefinitionTyped<C_OP_ConstrainDistance>", "==",
     "a concrete registered function class (CParticleFunctionDefinitionTyped<T> : CParticleFunctionDefinition : IParticleFunctionDefinition) adds no virtual: 3 entries, the same on every T, RE7 section 3.2"),
    (PLATFORM, "scenesystem", "ISceneSystem", "scenesystem.dll", "SV_SCENESYSTEM_VT", "==+1",
     "CSceneSystem (RTTI col 0) implements the 198-slot interface and adds exactly one virtual of its own, its scalar deleting destructor at slot 198 (ISceneSystem declares no destructor): 199 entries, RE3 section 4"),
    (PLATFORM, "scenesystem", "ISceneUtils", "scenesystem.dll", "SV_SCENEUTILS_VT", "==",
     "CSceneUtils (RTTI col 0) implements the interface as its single base and adds no virtual: 62 entries, RE3 section 4"),
    (PLATFORM, "scenesystem", "ISceneView", "scenesystem.dll", "SV_SCENEVIEW_VT", "==",
     "CSceneView (RTTI col 0) implements the interface and adds no virtual: 112 entries, the 111 virtuals plus ISceneView's inline destructor, RE3 section 4"),
    (PLATFORM, "scenesystem", "IRenderingPipeline", "scenesystem.dll", "SV_PIPELINE_ALIAS_VT", "==",
     "CRenderingPipelineAlias (RTTI col 0) forwards every slot to the aliased pipeline and adds no virtual: 14 entries (CRenderingPipelineForward has the same 14), RE3 section 4"),
    (PLATFORM, "client", "CClientInput", "client.dll", "CClientInput", "==",
     "CClientInput (RTTI col 0) is the root of the client input classes: its table is the destructor plus 32 virtuals, 33 entries (RE8 section 3.1)"),
    (PLATFORM, "engine", "IKeybindChangeListener", "client.dll", "CCSGOInput::IKeybindChangeListener", "==",
     "CCSGOInput's IKeybindChangeListener subobject (RTTI col 0xB80): a one-entry table that ends where the next COL begins, RE8 section 3.2"),
    (PLATFORM, "engine", "IInputHandler", "client.dll", "CCSGOInput::IInputHandler", "==",
     "CCSGOInput's IInputHandler subobject (RTTI col 0xB88): a one-entry table that ends where the next COL begins, RE8 section 3.2"),
    (PLATFORM, "toolutils2", "IBindTarget", "pet.dll", "CBaseEditorApp::IBindTarget", "==",
     "CBaseEditorApp's IBindTarget subobject (RTTI col 0x30): a nine-entry table, the destructor and eight virtuals, that ends where the next COL begins, RE9 section 3.1"),
    (PLATFORM, "toolutils2", "IToolsResourceListener", "pet.dll", "CPetApp::IToolsResourceListener", "==",
     "CPetApp's IToolsResourceListener subobject (RTTI col 0x1F0): a two-entry table that ends where the next COL begins, RE9 section 3.2"),
    (PLATFORM, "toolutils2", "IUndoable", "pet.dll", "CBaseUndoable", "==+1",
     "CBaseUndoable (RTTI col 0) is the 12-slot interface (IUndoable declares no destructor) plus its own destructor at slot 12: 13 entries, RE9 section 3.4"),
    (PLATFORM, "toolutils2", "CBaseUndoable", "pet.dll", "CBaseUndoable", "==",
     "CBaseUndoable's own table (RTTI col 0): the 12 slots of IUndoable and the destructor it declares, 13 entries, RE9 section 3.4"),
    (PLATFORM, "toolutils2", "IUndoListener", "pet.dll", "CPetDoc::IUndoListener", "==",
     "CPetDoc's IUndoListener subobject (RTTI col 8): a one-entry table that ends where the next COL begins, RE9 section 3.4"),
    (PLATFORM, "toolutils2", "CUndoHistory", "pet.dll", "PETV_UNDOHIST_VT", "==",
     "CUndoHistory's own table (RTTI col 0): the destructor and three virtuals, 4 entries, RE9 section 3.4"),
    (PLATFORM, "toolutils2", "IToolAttr_Table", "pet.dll", "CBaseKV3DataWithFGDClass", "==+6",
     "CBaseKV3DataWithFGDClass (RTTI col 0) is the 11-slot interface (IToolAttr_Table declares no destructor) plus its destructor and 5 virtuals of its own: 17 entries, RE9 section 3.5"),
    (PLATFORM, "toolutils2", "CBaseKV3DataWithFGDClass", "pet.dll", "CBaseKV3DataWithFGDClass", "==",
     "CBaseKV3DataWithFGDClass's own table (RTTI col 0): 17 entries, RE9 section 3.5"),
    (PLATFORM, "toolutils2", "IToolAttrNotifyRoot", "pet.dll", "CBasePetNode::IToolAttrNotifyRoot", "==",
     "CBasePetNode's IToolAttrNotifyRoot subobject (RTTI col 8): a six-entry table that ends where the next COL begins, RE9 section 3.5"),
    (PLATFORM, "toolutils2", "CBaseEditorSession", "pet.dll", "CBaseEditorSession", "==",
     "CBaseEditorSession's own table (RTTI col 0): the destructor and 25 virtuals, 26 entries, RE9 section 3.3"),
    (PLATFORM, "toolutils2", "CBaseEditorApp", "pet.dll", "CBaseEditorApp", "==",
     "CBaseEditorApp's own table (RTTI col 0): IToolSystem2's 17 slots and 32 virtuals of its own, 49 entries, RE9 section 3.2"),
    (PLATFORM, "toolutils2", "CBaseEditorSharedWindowApp", "pet.dll", "PETV_APP_VT", "==",
     "CPetApp (RTTI col 0, the ToolSystem2_001 singleton's class) adds no virtual to CBaseEditorSharedWindowApp: 50 entries, RE9 section 3.2"),
    (PLATFORM, "pet", "CBasePetNode", "pet.dll", "CBasePetNode", "==",
     "CBasePetNode's own table (RTTI col 0): 34 entries; the four node classes override but add none, RE9 section 3.5"),
    (PLATFORM, "pet", "CPetDoc", "pet.dll", "PETV_DOC_VT", "==",
     "CPetDoc's own table (RTTI col 0): CBaseUndoable's 13 entries and its own unk021, 14 entries, RE9 section 3.4"),
    (PLATFORM, "pet", "CPetSession", "pet.dll", "PETV_SESSION_VT", "==",
     "CPetSession's own table (RTTI col 0): CBaseEditorSession's 26 entries and its own unk031, 27 entries, RE9 section 3.3"),
    (PLATFORM, "toolutils2", "CBaseEditorApp", "modeldoc_editor.dll", "MDV_APP_VT", "==",
     "CModelDocEditorApp (RTTI col 0, the ToolSystem2_ModelDoc singleton's class) overrides slots but adds no virtual to CBaseEditorApp: 49 entries, RE10 section 3.1"),
    (PLATFORM, "toolutils2", "CBaseEditorSession", "modeldoc_editor.dll", "MDV_SESSION_VT", "==",
     "CModelDocEditorSession (RTTI col 0) overrides slots but adds no virtual to CBaseEditorSession: 26 entries, RE10 section 3.2"),
    (PLATFORM, "toolutils2", "IBindTarget", "modeldoc_editor.dll", "CModelDocEditorSession::IBindTarget", "==",
     "CModelDocEditorSession's IBindTarget subobject (RTTI col 0x68): a nine-entry table that ends where the next COL begins, RE10 section 3.5"),
    (PLATFORM, "toolutils2", "IToolsResourceListener", "modeldoc_editor.dll", "CModelDocEditorSession::IToolsResourceListener", "==",
     "CModelDocEditorSession's IToolsResourceListener subobject (RTTI col 0x80): a two-entry table, RE10 section 3.4"),
    (PLATFORM, "modeldoc_editor", "IAssetFingerprintListener", "modeldoc_editor.dll", "CModelDocEditorSession::IAssetFingerprintListener", "==",
     "CModelDocEditorSession's IAssetFingerprintListener subobject (RTTI col 0x78): a one-entry table that ends where the next COL begins, RE10 section 3.3"),
    (PLATFORM, "modeldoc_editor", "IErrorListener", "modeldoc_editor.dll", "CModelDocAnalysis", "==",
     "CModelDocAnalysis (RTTI col 0, embedded in CModelDocEditorSession) overrides IErrorListener's six slots and adds none: 6 entries, WP7 item 4"),
    (PLATFORM, "datamodel", "IDataModel", "hammer.dll", "HV_DATAMODEL_VT", "==+1",
     "CDataModel (RTTI col 0, the hammer-internal datamodel) implements the 140-slot interface and adds only its scalar deleting destructor at the end: 141 entries; IDataModel declares no destructor, RE11b section 3.1"),
    (PLATFORM, "datamodel", "CDmElement", "hammer.dll", "HV_ROOT_VT", "==",
     "CMapRootElement (RTTI col 0) overrides slots but adds no virtual to CDmElement: 15 entries, RE11b section 3.2"),
    (PLATFORM, "toolutils2", "CUndoHistory", "hammer.dll", "HV_HISTORY_VT", "==",
     "CMapDocHistory (RTTI col 0) overrides slots but adds no virtual to CUndoHistory: 4 entries, RE11b section 3.3"),
    (PLATFORM, "mapdoclib", "CMapNode", "hammer.dll", "CMapNode", "==",
     "CMapNode's own table (RTTI col 0): 252 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapAtom", "hammer.dll", "CMapAtom", "==",
     "CMapAtom's own table (RTTI col 0): 17 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapPoint", "hammer.dll", "CMapPoint", "==",
     "CMapPoint's own table (RTTI col 0): CMapAtom's 17 entries and its own eleven, 28 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapEntityIONode", "hammer.dll", "CMapEntityIONode", "==",
     "CMapEntityIONode's own table (RTTI col 0): 267 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapGameDataNode", "hammer.dll", "HV_MAPWORLD_VT", "==",
     "CMapWorld (RTTI col 0) overrides slots but adds no virtual to CMapGameDataNode: 273 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapEntity", "hammer.dll", "HV_MAPENTITY_VT", "==",
     "CMapEntity (RTTI col 0): CMapGameDataNode's 273 entries and its own unk061, 274 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapMeshBase", "hammer.dll", "CMapMeshBase", "==",
     "CMapMeshBase's own table (RTTI col 0): 257 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "CMapMesh", "hammer.dll", "HV_MAPMESH_VT", "==",
     "CMapMesh (RTTI col 0): CMapMeshBase's 257 entries and its own six, 263 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IToolRenderCallbacks", "hammer.dll", "CMapGroup::IToolRenderCallbacks", "==",
     "CMapGroup's IToolRenderCallbacks subobject (RTTI col 0xD0): a six-entry table that ends where the next COL begins, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IHitTestable", "hammer.dll", "CMapGroup::IHitTestable", "==",
     "CMapGroup's IHitTestable subobject (RTTI col 0xD8): an eight-entry table, RE11a section 1"),
    (PLATFORM, "mapdoclib", "ITransformable", "hammer.dll", "CMapGroup::ITransformable", "==",
     "CMapGroup's ITransformable subobject (RTTI col 0xE0): a four-entry table, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IMapNodeToolAttrRoot", "hammer.dll", "CMapGroup::IMapNodeToolAttrRoot", "==",
     "CMapGroup's IMapNodeToolAttrRoot subobject (RTTI col 0xE8): IToolAttr_Table's 11 entries and its own, 12 entries, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IRaytraceEnvironmentOwner", "hammer.dll", "CMapGroup::IRaytraceEnvironmentOwner", "==",
     "CMapGroup's IRaytraceEnvironmentOwner subobject (RTTI col 0xF8): a seven-entry table, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IVDataLookupListener", "hammer.dll", "CMapWorld::IVDataLookupListener", "==",
     "CMapWorld's IVDataLookupListener subobject (RTTI col 0x528): a one-entry table, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IHammerMeshChangeListener", "hammer.dll", "CMapMesh::col_3BA0", "==",
     "CMapMesh's IHammerMeshChangeListener subobject (RTTI col 0x3BA0): a three-entry table, RE11a section 1"),
    (PLATFORM, "mapdoclib", "IMapFileReference", "hammer.dll", "HV_DOC_VT", "==",
     "CMapDoc (RTTI col 0) overrides IMapFileReference's seven slots and adds no virtual: 7 entries, RE11b section 3.5"),
    (PLATFORM, "mapdoclib", "IUndoDoc", "hammer.dll", "CMapDoc::col_8", "==",
     "CMapDoc's IUndoDoc subobject (RTTI col 8): a three-entry table, RE11b section 3.5"),
    (PLATFORM, "toolutils2", "IUndoListener", "hammer.dll", "CMapDoc::col_10", "==",
     "CMapDoc's IUndoListener subobject (RTTI col 0x10): a one-entry table, RE11b section 3.5"),
    (PLATFORM, "hammer", "IMapDocSession", "hammer.dll", "HV_DOCSESSION_VT", "==+1",
     "CMapDocSession (RTTI col 0) implements the four-slot interface and adds only its scalar deleting destructor: 5 entries (IMapDocSession declares no destructor), RE11b section 3.4"),
    (PLATFORM, "hammer", "CMapDocSession", "hammer.dll", "HV_DOCSESSION_VT", "==",
     "CMapDocSession's own table (RTTI col 0): the interface's 4 entries and the destructor, 5 entries, RE11b section 3.4"),
    (PLATFORM, "hammer", "CHammerEditorSession", "hammer.dll", "HV_SESSION_VT", "==",
     "CHammerEditorSession's own table (RTTI col 0): CBaseEditorSession's 26 entries and GetMapFilePath, 27 entries, RE11b section 3.4"),
    (PLATFORM, "toolutils2", "IBindTarget", "hammer.dll", "CHammerEditorSession::IBindTarget", "==",
     "CHammerEditorSession's IBindTarget subobject (RTTI col 0x58): a nine-entry table, RE11b section 1"),
    (PLATFORM, "toolutils2", "IToolsResourceListener", "hammer.dll", "CHammerEditorSession::IToolsResourceListener", "==",
     "CHammerEditorSession's IToolsResourceListener subobject (RTTI col 0x68): a two-entry table, RE11b section 1"),
    (PLATFORM, "hammer", "IBindableCommandListener", "hammer.dll", "CHammerEditorSession::IBindableCommandListener", "==",
     "CHammerEditorSession's IBindableCommandListener subobject (RTTI col 0x70): a one-entry table, RE11b section 1"),
    (PLATFORM, "hammer", "IAssetFileListener", "hammer.dll", "CHammerEditorSession::IAssetFileListener", "==",
     "CHammerEditorSession's IAssetFileListener subobject (RTTI col 0x78): a one-entry table, RE11b section 1"),
    (PLATFORM, "toolutils2", "IToolRenderableViewHost", "hammer.dll", "HV_MAPVIEW_VT", "==",
     "CMapView (RTTI col 0) implements the interface and adds no virtual: 18 entries, RE11b section 3.6"),
    (PLATFORM, "toolutils2", "IBindTarget", "hammer.dll", "CMapView::IBindTarget", "==",
     "CMapView's IBindTarget subobject (RTTI col 8): a nine-entry table, RE11b section 1"),
    (PLATFORM, "toolutils2", "IBindableCameraScene", "hammer.dll", "CMapView::IBindableCameraScene", "==",
     "CMapView's IBindableCameraScene subobject (RTTI col 0x18): an eighteen-entry table, RE11b section 3.6"),
    (PLATFORM, "hammer", "ITagVisualizationListener", "hammer.dll", "HV_MAPVIEWMGR_VT", "==",
     "CMapViewMgr (RTTI col 0) implements the one-slot interface and adds no virtual: 1 entry, RE11b section 3.6"),
    (PLATFORM, "hammer", "IShaderReloadListener", "hammer.dll", "CMapViewMgr::IShaderReloadListener", "==",
     "CMapViewMgr's IShaderReloadListener subobject (RTTI col 8): a two-entry table, RE11b section 3.6"),
    (PLATFORM, "hammer", "IBoundingBoxChangeListener", "hammer.dll", "CMapViewMgr::IBoundingBoxChangeListener", "==",
     "CMapViewMgr's IBoundingBoxChangeListener subobject (RTTI col 0x10): a two-entry table, RE11b section 3.6"),
]

# NOTE (concrete-class vtable-length rows): a row for a class that implements an
# upstream interface and is located by its own RTTI COL is just one more row above:
# (PLATFORM, <ir group>, <interface>, <module>, <kind=vtable rtti entry of the
# concrete class>, "==" or ">=", why). "==" additionally claims the concrete class
# adds no virtual (the rule's `why` must say where that is established).

# Why each remaining generated vtable class is NOT checked (no pair above).
UNPAIRED_SKIP = {
    ("windows", "ISource2Engine"):
        "an abstract base of IVEngineClient2 and IVEngineServer2: no class in the staged binaries has it as its own table (CEngineClient's 185 entries are IVEngineClient2 == 18 + 167, checked on that row)",
    ("windows", "IGameEvent"):
        "the DB has no kind=vtable entry whose RTTI class locates an IGameEvent implementation",
    ("windows", "IGameEventListener2"):
        "the DB has no kind=vtable entry whose RTTI class locates a listener implementation",
    ("windows", "IGameEventManager2"):
        "the DB has no kind=vtable entry whose RTTI class provably implements the manager (a guess would be a silent mis-assert)",
    ("windows", "ISoundEventListener"):
        "a callback interface the sound operator system calls: no class in the staged binaries implements it (no RTTI, no string), so there is no table to locate (RE4 section 3.3)",
    ("windows", "CCSGOInput"):
        "upstream declares no virtual of its own for CCSGOInput (it only overrides CClientInput's), so clang emits no vftable length for it; its primary table is checked as the CVC_INPUT_VT_SLOTS sum row (CV_CSGOINPUT_VT against the generated CClientInput length) and the two secondary views have their own rows",
    ("windows", "CSchemaSystemTypeScope"):
        "upstream is a forward declaration only - clang emits no vftable for the class (generated S2_VTLEN_CSchemaSystemTypeScope is a comment) - and the game class implements ISchemaSystemTypeScope (checked via SSV_TYPESCOPE_VT)",
}   # the linux mode does not use this table: check_vtables_linux probes the staged
    # modules' symbol tables instead, so every linux SKIP states the actual evidence.

# Linux twin of VT_PAIRS for classes whose vtable the DB reaches through code (no
# exported `_ZTV`): (IR interface, libtier0-style module, kind=const vtlen entry, why) or
# (IR interface, module, entry, rule, why) with an explicit rule.
# Rule `==` (the 4-tuple default): the entry counts the class' own single table from its address
# point, and the claim is that the class adds no virtual to the interface (RE spec read each body).
# Rule `==+N`: the same, plus exactly N entries the compiler (not the source) adds to the primary
# table, which `why` names (GCC's entry for the override of a secondary base).
LINUX_VT_PAIRS = [
    ("IRenderService", "libengine2.so", "VTLEN_CRenderService", "==+1",
     "CRenderService's primary Itanium table (address point found from the function that stores it) is the interface's 40 slots plus the one entry GCC adds for CRenderService's override of its secondary base (RE5 section 0.1)"),
    ("IInputService", "libengine2.so", "VTLEN_CInputService", "==+1",
     "CInputService's primary Itanium table (address point found from the function that stores it) is the interface's 64 slots plus the one entry GCC adds for CInputService's override of its secondary base (RE5 section 0.1)"),
    ("ISource2Server", "libserver.so", "VTLEN_CSource2Server", "==+1",
     "CSource2Server's primary Itanium table (address point found from the static initializer that stores it into the Source2Server001 singleton) is the interface's 103 slots plus the one entry GCC adds for the override of its secondary base (object+0x28, a one-slot table)"),
    ("IVEngineClient2", "libengine2.so", "VTLEN_CEngineClient", "==",
     "CEngineClient's single Itanium table (address point found from the function that stores it) has the interface's 185 slots: single inheritance, no destructor, no added virtual (RE5 section 4)"),
    ("IRenderDevice", "librendersystemvulkan.so", "VTLEN_CRenderDeviceVulkan", "==+40",
     "CRenderDeviceVulkan's primary Itanium table (address point found from the complete-object destructor) is the interface's 255 slots plus 40 entries of its own: CRenderDeviceBase's two destructor entries and 35 virtuals, then 3 of CRenderDeviceVulkan's own, 295 entries (RE6 section 4)"),
    ("IParticleSystemMgr", "libparticles.so", "VTLEN_CParticleSystemMgr", "==+2",
     "CParticleSystemMgr's primary Itanium table (address point found from the function that stores it) is the interface's 61 slots plus the 2 entries GCC adds for CParticleSystemMgr's overrides of its secondary bases ISceneObjectListener and IToolsResourceListener (RE7 section 4)"),
    ("CClientInput", "libclient.so", "VTLEN_CClientInput",
     "CClientInput's single Itanium table (address point found from its complete-object destructor) has 34 entries: the destructor's two plus the 32 virtuals (RE8 section 3.1)"),
    ("IKeybindChangeListener", "libclient.so", "VTLEN_CCSGOInput_IKeybindChangeListener",
     "CCSGOInput's IKeybindChangeListener subobject table (offset-to-top -0xB80) has the interface's one slot (RE8 section 3.2)"),
    ("IInputHandler", "libclient.so", "VTLEN_CCSGOInput_IInputHandler",
     "CCSGOInput's IInputHandler subobject table (offset-to-top -0xB88) has the interface's one slot (RE8 section 3.2)"),
    ("ISoundEventManager", "libsoundsystem.so", "VTLEN_ISoundEventManager",
     "CSoundOpSystem's ISoundEventManager subobject (offset-to-top -8, found through the constructor's second vptr) has the interface's 63 slots: no destructor, no added virtual (RE4 section 0)"),
    ("ITestScriptMgr", "libtier0.so", "VTLEN_CTestScriptMgr",
     "CTestScriptMgr's single Itanium table (address point found from its complete-object destructor) has 19 slots: the 17 shared with the MSVC table plus the two destructor slots"),
    ("ISceneUtils", "libscenesystem.so", "VTLEN_CSceneUtils",
     "CSceneUtils' single Itanium table (address point found from a constructor) has the interface's 62 slots: no destructor, no added virtual (RE3 section 4)"),
    ("ISceneView", "libscenesystem.so", "VTLEN_CSceneView",
     "CSceneView's single Itanium table (address point found from a constructor) has 113 slots: the 111 virtuals plus the two entries of ISceneView's inline destructor (RE3 section 4)"),
    ("IRenderingPipeline", "libscenesystem.so", "VTLEN_CRenderingPipelineAlias",
     "CRenderingPipelineAlias' single Itanium table (address point found from the allocation that stores it) has the interface's 14 slots (RE3 section 4)"),
]

# Classes the Linux lane has a better reason to skip than the generic "no _ZTV symbol" text.
LINUX_UNPAIRED_SKIP = {
    "IEngineService":
        "the Linux build emits no table for the interface itself: every concrete service has its own table with the "
        "destructor pair in slots 22 and 23 (RE5 section 0.1), and the services' rows (IRenderService, IInputService) "
        "include them; the Windows lane checks IEngineService's own 23-entry table",
    "ISource2Engine":
        "an abstract base of IVEngineClient2 and IVEngineServer2: no class has it as its own table, CEngineClient's "
        "185 entries are checked on the IVEngineClient2 row",
    "ISoundOpSystem":
        "CSoundOpSystem's primary Itanium table is 166 entries long: the interface's 101 slots, then 63 entries GCC adds for "
        "the overrides of the secondary base ISoundEventManager and the class's own two LoadManifestStacksForParsing "
        "overloads (RE4 section 0); the abstract CTier2AppSystem<ISoundOpSystem> table that shows exactly 101 has no "
        "locator in the sigs DB, so the interface length is checked as the sum row const:libsoundsystem.so/SNC_SOS_VT_SLOTS",
    "ISoundEventListener":
        "a callback interface the sound operator system calls: no class in the staged binaries implements it, so there is no table to measure (RE4 section 3.3)",
    "IParticleFunctionDefinition":
        "the concrete CParticleFunctionDefinitionTyped<T> tables are exported by no symbol and referenced only by the "
        "static initializer that registers every function class, whose bytes repeat for each class: the sigs DB has no "
        "unique locator for one (libparticles.json); the Windows lane checks the 3-entry table (RE7 section 3.2)",
    "CCSGOInput":
        "upstream declares no virtual of its own for CCSGOInput, so there is no generated length to compare; its 36-entry "
        "primary table is checked as the sum row const:libclient.so/VTLEN_CCSGOInput (CClientInput's 34 + 2)",
    "ISceneSystem":
        "CSceneSystem's Itanium table is 203 entries long: the 198 interface slots, then the entries GCC adds for "
        "the overrides of three secondary bases and CSceneSystem's own two destructor entries (RE3 section 4), so no `==` "
        "claim about the interface's length can be made from it and the lane has no `>=` rule; no sigs entry "
        "locates that table (the spec gives no signature for it); the Windows lane checks CSceneSystem's "
        "table with `==+1`",
}

# (platform, module, sigs const entry, generated const name, why).
CONST_PAIRS = [
    (PLATFORM, "engine2.dll", "S2_ATTR_TOKEN_COLOR", "TOKEN_OUTPUT_COLOR",
     "the sigs locator S2_ATTR_TOKEN_COLOR points at the game's color-token value; the IR measured TOKEN_OUTPUT_COLOR from the upstream headers"),
    (PLATFORM, "engine2.dll", "S2_ATTR_TOKEN_DEPTH", "TOKEN_OUTPUT_DEPTH",
     "same for the depth token"),
]

# Sigs consts that are literals (no locator: the engine's ImageFormat name table is data the
# DB has no anchor for) and have a generated counterpart: (platform, module, sigs const entry,
# generated const name, why). Equality is reported SKIP (LITERAL, not measured in a binary; the
# header holds the same number) and a difference is a FAIL.
LITERAL_CONST_PAIRS = [
]

# Locates consts with no generated counterpart -> SKIP (no upstream side exists;
# the value itself is expect-checked by `sigcheck.py resolve --strict`).
CONST_UNPAIRED = []
# GSVC_GAMECONFIG_VT_SLOTS is deliberately absent from both const tables: it is
# the `==` (a) row's game side (see VT_PAIRS), not a third line.

# The linux half of CONST_UNPAIRED: every located linux const that has no generated
# counterpart. None today. (The libtier0 VTLEN_* consts are not here: they are (a)'s
# DB-measured vtable-length rows, check_vtables_linux; libsoundsystem's SNC_SOS_VT_SLOTS
# and SNOFF_SEP_* are CONST_LENGTH_SUMS / OFFSET_PAIRS rows.)
CONST_UNPAIRED += []

# Sum rows: a sigs vtlen const (the concrete class' whole table) that the generated interface
# lengths explain: measured == sum of the IR lengths of `classes` (per platform ABI) + `extra`.
# (platform, module, sigs const entry, IR classes, extra, why).
CONST_LENGTH_SUMS = [
    (PLATFORM, "soundsystem.dll", "SNC_SOS_VT_SLOTS", ("ISoundOpSystem",), 2,
     "CSoundOpSystem's table = the 101-slot ISoundOpSystem interface + its own two LoadManifestStacksForParsing overloads; "
     "MSVC gives the overrides of the secondary base ISoundEventManager no slot (RE4 section 0)"),
    (PLATFORM, "client.dll", "CVC_INPUT_VT_SLOTS", ("CClientInput",), 0,
     "CCSGOInput's primary table (the pinned vftable the bridge copies) is CClientInput's 33 entries: it overrides 23 of them and adds none, and MSVC gives its overrides of the secondary bases IKeybindChangeListener and IInputHandler no slot (RE8 section 3.1)"),
    ("linux", "libclient.so", "VTLEN_CCSGOInput", ("CClientInput",), 2,
     "build 2000930: CCSGOInput's primary Itanium table = CClientInput's 34 entries + the 2 entries GCC emits for the overrides of the secondary bases IKeybindChangeListener and IInputHandler (RE8 section 3.1)"),
    ("linux", "libsoundsystem.so", "SNC_SOS_VT_SLOTS", ("ISoundOpSystem", "ISoundEventManager"), 2,
     "build 2000930: CSoundOpSystem's primary Itanium table = the 101-slot interface + 63 entries GCC emits for the overrides "
     "of the secondary base ISoundEventManager (each has a this-adjusting thunk in the secondary table) + the same two own "
     "overloads; the 162 recorded for build 2000908 is the same sum with a 97-slot interface (RE4 section 0)"),
]

# Params-struct offsets: a sigs offset entry vs the IR record field it measures.
# (platform, module, sigs offset entry, IR group, record, field, why).
OFFSET_PAIRS = [
    ("linux", "libsoundsystem.so", "SNOFF_SEP_HASH", "soundsystem", "SosStartSoundEventParams_t", "m_nSoundEventHash",
     "the same offset on the Itanium side (the params block is identical on both ABIs)"),
    ("linux", "libsoundsystem.so", "SNOFF_SEP_ENTINDEX", "soundsystem", "SosStartSoundEventParams_t", "m_SourceEntity",
     "the same offset on the Itanium side"),
]

# Secondary-base offsets: a sigs offset entry (RTTI offset-to-top) vs the IR's recorded offset of
# the base subobject in the class. (platform, module, sigs offset entry, IR class, base, why).
BASE_OFFSET_PAIRS = [
]

# (i) the entity-identity chunk geometry, linux lane (check_identity; the windows run
# emits no rows - the windows geometry facts are the DB's own, checked by
# `sigcheck.py resolve --strict`). (platform, module, entry, why). The check is generic,
# the rows are documented knowledge, exactly like VT_PAIRS.
IDENTITY_ENTRIES = [
    ("linux", "libclient.so", "CG_ENTITY_CHUNKS",
     "the chunk-pointer array the identity lookup walks (512 identities per chunk, chunk "
     "index clamped to 64 chunks); a position, not a number - no generated counterpart "
     "(the identity list is a game-internal class, not an upstream interface)"),
]
# The lookup's chunk bits (index >> 9, mask 0x1FF) are a SHIFT; upstream spells the chunk size
# (MAX_ENTITIES_IN_LIST, 512, public/entity2/entityidentity.h; the allowlist const of the same
# name), so an `sdk` expression cannot express log2 and the entry stays a measurement. This row is
# what keeps the binary check: `1 << measured == S2_CONST_MAX_ENTITIES_IN_LIST`. The bridge
# (mcp_bridge_game.inc gs_ident) divides/masks by the generated constant and _Static_asserts it is a
# power of two. (platform, module, entry, generated const)
IDENTITY_BITS = [
    ("linux", "libclient.so", "CCONST_IDENTITY_CHUNK_BITS", "MAX_ENTITIES_IN_LIST"),
    ("windows", "client.dll", "CCONST_IDENTITY_CHUNK_BITS", "MAX_ENTITIES_IN_LIST"),
]
# The stride row: the identity lookup's `imul 0x70` (= 112) vs the IR record named here - its
# Itanium size is the upstream side (S2_SIZEOF_CEntityIdentity pins the MSVC twin). The DB entry
# that measures it is libclient.json CCONST_IDENTITY_STRIDE (kind=const, name containing STRIDE);
# when such an entry is present this row compares `==`, and when none is it SKIPs naming the entry
# it expects (a renamed/removed entry must not look like a pass).
IDENTITY_STRIDE = ("linux", "entity", "CEntityIdentity")   # (platform, IR group, record)

# The linux trees: where the Linux build is staged (sigs/README.md "Linux binaries").
def linux_bins_root(tree=None):
    return Path(tree) if tree else Path(os.environ.get("CS2_LINUX_BINS", sc.ROOT / "target/game-bins-linux-2000930"))


# ---------------------------------------------------------------------------
# loading


def load_ir():
    ir = {}
    for f in sorted(IR_DIR.glob("*.json")):
        ir[f.stem] = json.loads(f.read_text())
    return ir


class Probes:
    """One parsed binary + Resolver per module, built on demand (cache). The platform
    picks the binary layout: PE under $CS2_BINS (windows), ELF64 under the Linux tree
    (linux) - both through sigcheck's shared readers (parse_pe / parse_elf, chosen by
    magic so a tree never parses a binary with the wrong reader)."""

    def __init__(self, db, platform="windows", tree=None):
        self.db = db
        self.platform = platform
        self.tree = tree
        self.mods = {}   # module -> file doc
        for f, d in db.items():
            self.mods[d["module"]] = d
        self.cache = {}

    def get(self, mod):
        if mod not in self.cache:
            p = sc.module_path(mod, root=self.tree, platform=self.platform)
            if not p.exists():
                raise LookupError("%s: binary %s missing (stage it: sigs/README.md"
                                  " 'Binaries' / 'Linux binaries')" % (mod, p))
            data = p.read_bytes()
            img = sc.parse_elf(data) if data[:4] == b"\x7fELF" else sc.parse_pe(data)
            r = sc.Resolver(data, img)
            self.cache[mod] = (data, img, r)
        return self.cache[mod]

    def resolve(self, mod, name):
        data, pe, r = self.get(mod)
        return r.resolve(name, self.mods[mod]["entries"][name], self.mods[mod]["entries"])


def rtti_of_vtable(data, pe, vt):
    """(type-descriptor name, COL offset) of the COL that owns the vftable at `vt`,
    from sigcheck's RTTI index - the read-back of what the locator matched."""
    for (name, off), rvs in sc.rtti_index(data, pe).items():
        if vt in rvs:
            return name, off
    return None, None


def expected_tdname(cls):
    """The TypeDescriptor spelling sigcheck.rtti_vtable resolves a locator's class to."""
    return cls if cls.startswith(".?A") else ".?AV%s@@" % cls


# ---------------------------------------------------------------------------
# the checks - each appends (status, tag, text) rows; status PASS/FAIL/SKIP


def ir_vtable_classes(ir):
    """{class: (group, vtable)} of every IR vtable (one line per class in the report)."""
    vt_by_class = {}
    for group, d in ir.items():
        if group in ("consts",):   # the consts IR group carries no vtables anyway
            continue
        for vt in d.get("vtables", []):
            cls = vt["class"]
            if cls in vt_by_class:
                raise RuntimeError("IR has two vtables named %s" % cls)
            vt_by_class[cls] = (group, vt)
    return vt_by_class


def itanium_len(ir, cls):
    """upstream's Itanium length of `cls`'s vtable, from the single source (gen.py's
    walk, the number the emitted vtable struct is sized by); None when the IR has no
    vtable for the class or its slots are not the dense set the rule needs."""
    try:
        return gen.itanium_vtable_len(ir, cls)
    except (gen.GenError, LookupError, KeyError):
        return None


WINDOWS_ONLY_WHY = (
    "[%s] is a Windows-only group (no Linux build of these modules exists): its Itanium "
    "numbers only prove the header compiles under both ABIs and claim nothing about a binary"
)


def windows_only_groups(ir):
    """{group: ir group doc} for every IR group marked `"platforms": ["windows"]`."""
    return {g: d for g, d in sorted(ir.items()) if d.get("platforms") == ["windows"]}


def windows_only_classes(ir):
    """{vtable class: group} over the Windows-only groups."""
    return {vt["class"]: g for g, d in windows_only_groups(ir).items() for vt in d.get("vtables", [])}


def check_platform_groups(platform, ir, rows):
    """linux: one SKIP row per Windows-only group (what it holds and why it is not
    checked). windows: nothing, the group is checked like any other."""
    if platform != "linux":
        return
    for g, d in windows_only_groups(ir).items():
        n = ", ".join("%d %s" % (len(d.get(k) or []), k)
                      for k in ("records", "specs", "enums", "vtables"))
        rows.append(("SKIP", "group:%s" % g, "-", "-", "%s (%s)" % (WINDOWS_ONLY_WHY % g, n)))


def ir_base_offset(ir, derived, base, _depth=0):
    """The MSVC offset of the `base` subobject inside `derived` according to the IR (a vtable
    class's `secondary_bases` msvc_offset, else the record's `bases`, summed down the chain), or
    None when the IR does not know `derived` or `base` is not one of its bases."""
    if _depth > 8:
        return None
    vt = ir_vtable_classes(ir).get(derived)
    if vt:
        for b in vt[1].get("secondary_bases", []):
            if b["name"] == base:
                return b["msvc_offset"]
    for d in ir.values():
        for rec in d.get("records", []):
            if rec["name"] != derived:
                continue
            offs = (rec.get("msvc") or {}).get("base_offsets")
            for i, b in enumerate(rec.get("bases", [])):
                off = offs[i] if offs else b["offset"]
                if b["name"] == base:
                    return off
                deeper = ir_base_offset(ir, b["name"], base, _depth + 1)
                if deeper is not None:
                    return off + deeper
    return None


# Hammer-internal classes that add no base to the IR class their table is a subobject of
# (RE11a section 1: CMapGroup/CMapWorld override slots and add none), so the IR class's base
# offsets are theirs.
DERIVED_AS = {"CMapGroup": "CMapNode", "CMapWorld": "CMapGameDataNode"}


def check_vtables(platform, ir, probes, rows):
    """(a) every generated vtable class vs the game table a sigs entry locates."""
    vt_by_class = ir_vtable_classes(ir)
    if platform == "linux":
        check_vtables_linux(ir, probes, vt_by_class, rows)
        return
    for cls, (group, vt) in sorted(vt_by_class.items()):
        upstream = (vt.get("msvc") or {}).get("length")
        pair_rows = [p for p in VT_PAIRS if p[0] == platform and p[2] == cls]
        if not pair_rows:
            why = UNPAIRED_SKIP.get((platform, cls),
                                    UNPAIRED_SKIP.get((platform, None)))
            rows.append(("SKIP", "vt:%s" % cls, str(upstream), "-", why))
            continue
        for _, _, _, mod, entry, rule, why in pair_rows:
            tag = "vt:%s@%s/%s" % (cls, mod, entry)
            try:
                if upstream is None:
                    rows.append(("SKIP", tag, "none", "-", why + "; upstream has no vftable length (clang emitted none)"))
                    continue
                doc = probes.mods[mod]
                # the sigs entry must actually be what the pair table claims
                ent = doc["entries"][entry]
                if ent.get("kind") != "vtable" or (ent.get("locator") or {}).get("type") != "rtti":
                    rows.append(("FAIL", tag, str(upstream), "-",
                                 "pair table misconfigure: %s is not a kind=vtable + rtti entry" % entry))
                    continue
                vt_rva = probes.resolve(mod, entry)
                data, pe, _ = probes.get(mod)
                name, coloff = rtti_of_vtable(data, pe, vt_rva)
                want_name = expected_tdname(ent["locator"]["class"])
                want_col = ent["locator"].get("col_offset", 0)
                if name != want_name or coloff != want_col:
                    rows.append(("FAIL", tag, str(upstream), "RTTI %s@%d at vt %#x" % (name, coloff, vt_rva),
                                 "the located table's COL does not name %s@%d" % (want_name, want_col)))
                    continue
                if want_col:
                    # a secondary table: the COL offset-to-top is where the base sits in the
                    # object, and the generated header must say the same. A wrong base in a
                    # header leaves the length row above green, so compare the offsets too.
                    derived = ent["locator"]["class"]
                    ir_off = ir_base_offset(ir, DERIVED_AS.get(derived, derived), cls)
                    if ir_off is not None:
                        ok_base = ir_off == want_col
                        rows.append(("PASS" if ok_base else "FAIL", "base:%s/%s@%s/%s" % (derived, cls, mod, entry),
                                     "%#x" % ir_off, "%#x" % want_col,
                                     "%s's %s subobject: the COL of %s says +%#x" % (derived, cls, entry, want_col)
                                     if ok_base else
                                     "%s's %s subobject is at %#x in the generated layout, the binary's COL says %#x"
                                     % (derived, cls, ir_off, want_col)))
                game = sc.vtable_len(data, pe, vt_rva)
                want = upstream + int(rule[3:]) if rule.startswith("==+") else upstream
                ok = (game == want) if rule.startswith("==") else (game >= upstream)
                status = "PASS" if ok else "FAIL"
                why_full = why if ok else (
                    "%s [rule %s: %s]" % (why, rule,
                     "an extra slot means the table moved - locate it in the binary before touching anything"
                     if rule == ">=" else
                     "the exact-implementation row grew or shrank: the upstream header is stale vs build %s (upstream PR path, game-update lane A)" % sc.CURRENT_BUILD))
                rows.append((status, tag, ("%d+%d" % (upstream, want - upstream)) if want != upstream else str(upstream), str(game), why_full))
            except (LookupError, ValueError, AssertionError, IndexError) as ex:
                rows.append(("FAIL", tag, str(upstream or "none"), "-", "game side unresolved: %s" % ex))


def ztv_symbols(ir, probes):
    """{class: (module, symbol)} - which IR vtable classes have an exported `_ZTV`
    symbol anywhere in the staged Linux modules' symbol tables. The evidence behind
    the linux (a) SKIP rows: the same symbolization fact linuxvtables.py surveys,
    read through sigcheck's shared ELF reader (no second reader here)."""
    want = {sc.elf_vtable_symbol(cls): cls for cls in vt_by_class_keys(ir)}
    out = {}
    for mod in sorted(probes.mods):
        try:
            _data, img, _r = probes.get(mod)
        except LookupError:
            continue   # not staged: the DB rows name that instead, fail closed
        for sym, cls in want.items():
            if cls not in out and sym in img["symbols"]:
                out[cls] = (mod, sym)
    return out


def vt_by_class_keys(ir):
    for group, d in ir.items():
        for vt in d.get("vtables", []):
            yield vt["class"]


MEASURED_VTLEN_SKIP = (
    "upstream: no Itanium vtable for %s in the IR - the class is not in gen.py's "
    "allowlist, and gen.py's itanium_vtable_len (the only source for the `==` rule's "
    "upstream side) derives lengths from IR vtables; adding the class would change the "
    "generated IR, which is out of scope for a check script. The DB entry's own notes "
    "record the upstream number; this script does not consume prose as a fact"
)


def check_vtables_linux(ir, probes, vt_by_class, rows):
    """(a) on Linux: the DB's measured Itanium VTLENs vs upstream's Itanium lengths.

    Two row sources, both real:
    * DB-driven: every linux kind=const entry whose locator is an `itanium_vtable`
      with a `measure` resolves its measured length through the Resolver and
      compares with gen.py's itanium_vtable_len under rule `==` - an
      `itanium_vtable` locator names the class's own exact `_ZTV` symbol and
      `measure: "symbol"` reads the linker's st_size of that table group (header +
      every slot; unlike the code-pointer scan it cannot under-count a null last
      slot), so equality with upstream's Itanium length is the checkable claim.
      Where the IR has no vtable for the class the row SKIPs naming exactly what
      the upstream number would require (gen's allowlist; changing IR is out of
      scope).
    * IR-driven: every IR vtable class still gets its line, backed by the
      ztv_symbols evidence probe rather than a canned reason.
    """
    covered = set()
    measured = []
    wo_classes = windows_only_classes(ir)
    for mod in sorted(probes.mods):
        for name, ent in sorted(probes.mods[mod]["entries"].items()):
            loc = ent.get("locator") or {}
            if ent.get("kind") != "const" or loc.get("type") != "itanium_vtable" or not loc.get("measure"):
                continue
            measured.append((mod, name, ent, loc))
            if loc.get("class"):
                covered.add(loc["class"])
    for mod, name, ent, loc in measured:
        cls = loc.get("class")
        # the tag is entry-keyed (the class here is the DB entry's own claim, not an
        # authoritative IR name like the windows rows): when the class drifts the row
        # must stay attributable to the entry it came from.
        tag = "vtlen:%s/%s" % (mod, name)
        try:
            game = probes.resolve(mod, name)
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            rows.append(("FAIL", tag, "-", "-", "game side unresolved: %s" % ex))
            continue
        own = sc.elf_vtable_symbol(cls) if cls else None
        if cls in wo_classes:
            rows.append(("SKIP", tag, "-", str(game), WINDOWS_ONLY_WHY % wo_classes[cls]))
            continue
        if own is None:
            rows.append(("SKIP", tag, "none", str(game),
                         "the locator names only the mangled symbol, not the class - "
                         "no upstream side is derivable (name the class for the `==` row)"))
            continue
        if (loc.get("symbol") or own) != own:
            rows.append(("SKIP", tag, "-", str(game),
                         "the locator is not the class's own exact `_ZTV` symbol "
                         "(%s, a secondary view): only >= would be provable, and the "
                         "linux lane claims no >= row" % loc.get("symbol")))
            continue
        upstream = itanium_len(ir, cls)
        if upstream is None:
            rows.append(("SKIP", tag, "none", str(game), MEASURED_VTLEN_SKIP % cls))
            continue
        ok = game == upstream
        rows.append(("PASS" if ok else "FAIL", tag, str(upstream), str(game),
                     "rule ==: the locator is the class's own exact `_ZTV` symbol and "
                     "`measure: \"symbol\"` reads the linker's st_size of that table "
                     "group (no under-count) - an added or removed virtual changes it, "
                     "so measured == upstream's Itanium length" if ok else
                     "rule == FAILS: the game's `%s` group measures %d slots, upstream's "
                     "Itanium vtable is %d - a stale upstream header or a wrong IR "
                     "(upstream PR path, game-update lane A)" % (sc.elf_vtable_symbol(cls), game, upstream)))
    for pair in LINUX_VT_PAIRS:
        cls, mod, entry = pair[:3]
        rule, why = pair[3:] if len(pair) == 5 else ("==", pair[3])
        extra = int(rule[3:]) if rule.startswith("==+") else 0
        tag = "vt:%s@%s/%s" % (cls, mod, entry)
        if cls in wo_classes:
            rows.append(("SKIP", tag, "-", "-", WINDOWS_ONLY_WHY % wo_classes[cls]))
            covered.add(cls)
            continue
        try:
            game = probes.resolve(mod, entry)
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            rows.append(("FAIL", tag, "-", "-", "game side unresolved: %s" % ex))
            covered.add(cls)
            continue
        covered.add(cls)
        upstream = itanium_len(ir, cls)
        if upstream is None:
            rows.append(("SKIP", tag, "none", str(game), MEASURED_VTLEN_SKIP % cls))
            continue
        ok = game == upstream + extra
        rows.append(("PASS" if ok else "FAIL", tag, ("%d+%d" % (upstream, extra)) if extra else str(upstream), str(game),
                     "rule %s: %s" % (rule, why) if ok else
                     "rule %s FAILS: the game table has %d slots, upstream's Itanium vtable is %d%s"
                     % (rule, game, upstream, (" (+%d)" % extra) if extra else "")))
    ztv = ztv_symbols(ir, probes)
    staged = sorted(m for m in probes.mods if (sc.module_path(m, root=probes.tree, platform="linux")).exists())
    for cls in sorted(vt_by_class):
        if cls in covered:
            continue   # its line is the DB-measured row above
        if cls in wo_classes:
            rows.append(("SKIP", "vt:%s" % cls, "-", "-", WINDOWS_ONLY_WHY % wo_classes[cls]))
            continue
        upstream = itanium_len(ir, cls)
        if cls in LINUX_UNPAIRED_SKIP:
            why = LINUX_UNPAIRED_SKIP[cls]
        elif not staged:
            why = "no staged Linux module at all under the tree - nothing to check against"
        elif cls in ztv:
            mod, sym = ztv[cls]
            why = "exported as %s in %s, but the DB has no measured VTLEN entry for it (a sigs entry is the sigs owner's file); linuxvtables.py compares the symbol with the IR directly" % (sym, mod)
        else:
            why = ("no exported `_ZTV` symbol for the class in the staged Linux modules "
                   "(%s) - the game's own C++ symbols are hidden in .dynsym and every IR "
                   "vtable class is an abstract interface with no emitted vtable at all "
                   "(the survey linuxvtables.py prints), so no sigs entry can measure it; "
                   "the Itanium numbers are checked live by s2sdk-probe (RESULT ok stage=sdk)"
                   % ", ".join(staged))
        rows.append(("SKIP", "vt:%s" % cls, str(upstream) if upstream is not None else "none", "-", why))


def check_sdk(platform, probes, rows, defines):
    """(d) every sigs entry carrying `sdk`: the generated expression (evaluated against
    bridge/s2sdk_gen.h, which gen.py --check proves is the pinned upstream's) must equal the
    entry's measurement - the vslot locator's slot index, else the resolved value. These are
    the numbers the bridge compiles from the generated header instead of a pin, so this row
    and bridge/pins.py's refusal are what hold them to the binary. A located entry is a PASS;
    a literal (no locator: the number is a bridge-side shape no binary backs) is SKIP (LITERAL)
    on equality."""
    if platform != "windows":
        return   # sigcheck validate keeps `sdk` on Windows entries (s2sdk_gen.h is the MSVC side)
    for mod, doc in probes.mods.items():
        for name, ent in doc["entries"].items():
            expr = ent.get("sdk")
            if not expr:
                continue
            tag = "sdk:%s/%s" % (mod, name)
            try:
                want = sc.sdk_eval(expr, defines)
            except (LookupError, ValueError) as ex:
                rows.append(("FAIL", tag, "-", "-", str(ex)))
                continue
            try:
                game = sc.sdk_measured(ent, probes.resolve(mod, name))
            except (LookupError, ValueError, AssertionError, IndexError) as ex:
                rows.append(("FAIL", tag, str(want), "-", "entry unresolved on build %s: %s"
                             % (sc.CURRENT_BUILD, ex)))
                continue
            what = "%s = %s" % (expr, ent.get("notes", "")[:80])
            if game != want:
                rows.append(("FAIL", tag, str(want), str(game),
                             "the header and the game disagree (%s); fix the header and the entry "
                             "together (workflows/game-update.md A2b)" % expr))
            elif ent.get("locator"):
                rows.append(("PASS", tag, str(want), str(game), what))
            else:
                rows.append(("SKIP", tag, str(want), str(game),
                             "LITERAL: no locator, not measured in any binary; equals %s" % expr))


def check_consts(platform, ir, probes, rows):
    """(c) located sigs consts vs their generated S2_CONST_* counterparts."""
    consts = {c["name"]: c for c in ir.get("consts", {}).get("consts", [])}
    for _plat, mod, entry, cname, why in CONST_PAIRS:
        if _plat != platform:
            continue
        tag = "const:%s/%s" % (mod, entry)
        try:
            const = consts.get(cname)
            upstream = const["msvc"]["value"] if const and const.get("kind") == "int" else None
            if upstream is None:
                rows.append(("FAIL", tag, "-", "-", "no measured const %r in the IR (gen.py and the pair table disagree)" % cname))
                continue
            doc = probes.mods[mod]
            ent = doc["entries"][entry]
            if ent.get("kind") != "const" or not ent.get("locator"):
                rows.append(("FAIL", tag, str(upstream), "-", "pair table misconfigure: %s is not a located kind=const entry" % entry))
                continue
            game = probes.resolve(mod, entry)
            ok = game == upstream
            rows.append(("PASS" if ok else "FAIL", tag, str(upstream), str(game), why))
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            rows.append(("FAIL", tag, "-", "-", "game side unresolved: %s" % ex))
    for _plat, mod, entry, cname, why in LITERAL_CONST_PAIRS:
        if _plat != platform:
            continue
        tag = "const:%s/%s" % (mod, entry)
        try:
            const = consts.get(cname)
            upstream = const["msvc"]["value"] if const and const.get("kind") == "int" else None
            if upstream is None:
                rows.append(("FAIL", tag, "-", "-", "no measured const %r in the IR (gen.py and the pair table disagree)" % cname))
                continue
            game = probes.resolve(mod, entry)
            if game == upstream:
                rows.append(("SKIP", tag, str(upstream), str(game),
                             "LITERAL: %s sigs const, not measured in any binary (no locator: the format name table has no anchor); "
                             "equals the generated %s (%s)" % (mod, cname, why)))
            else:
                rows.append(("FAIL", tag, str(upstream), str(game),
                             "%s literal const differs from the generated %s (%s)" % (mod, cname, why)))
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            rows.append(("FAIL", tag, "-", "-", "literal const unresolved: %s" % ex))
    for _plat, mod, entry, why in CONST_UNPAIRED:
        if _plat != platform:
            continue
        tag = "const:%s/%s" % (mod, entry)
        try:
            value = probes.resolve(mod, entry)   # present + resolving stays a hard fact
            rows.append(("SKIP", tag, "-", str(value), why))
        except (LookupError, ValueError) as ex:
            rows.append(("FAIL", tag, "-", "-", "no upstream side, but the sigs entry does not resolve either: %s" % ex))


def _field_offset(ir, group, rec_name, field, key):
    rec = next(r for r in ir[group]["records"] if r["name"] == rec_name)
    return next(f["offset"] for f in rec[key]["fields"] if f["name"] == field)


def check_sums_and_offsets(platform, ir, probes, rows):
    """(c') the sum rows (a concrete class' whole vtable length explained by generated
    interface lengths) and the params-struct offset rows."""
    key = {"windows": "msvc", "linux": "itanium"}[platform]
    for _plat, mod, entry, classes, extra, why in CONST_LENGTH_SUMS:
        if _plat != platform:
            continue
        tag = "const:%s/%s" % (mod, entry)
        try:
            if platform == "windows":
                lens = []
                for cls in classes:
                    vt = ir_vtable_classes(ir)[cls][1]
                    lens.append((vt.get("msvc") or {}).get("length"))
            else:
                lens = [itanium_len(ir, cls) for cls in classes]
            if any(n is None for n in lens):
                rows.append(("FAIL", tag, "-", "-", "no %s vtable length in the IR for %s" % (key, list(classes))))
                continue
            upstream = sum(lens) + extra
            game = probes.resolve(mod, entry)
            ok = game == upstream
            expr = " + ".join("%s %d" % (c, n) for c, n in zip(classes, lens)) + " + %d own" % extra
            rows.append(("PASS" if ok else "FAIL", tag, str(upstream), str(game),
                         "%s [%s]" % (why, expr) if ok else
                         "rule == FAILS: the game table has %d entries, the generated lengths explain %d (%s): a stale "
                         "upstream header or a changed class" % (game, upstream, expr)))
        except (LookupError, ValueError, AssertionError, IndexError, KeyError) as ex:
            rows.append(("FAIL", tag, "-", "-", "game side unresolved: %s" % ex))
    for _plat, mod, entry, group, rec_name, field, why in OFFSET_PAIRS:
        if _plat != platform:
            continue
        tag = "off:%s/%s" % (mod, entry)
        try:
            if isinstance(field, tuple):
                # a member of an embedded object: the sum of the offsets along the chain, each
                # hop (IR group, record, field); `group` and `rec_name` are the first hop's
                upstream = sum(_field_offset(ir, g, r, f, key) for g, r, f in field)
                shown = "+".join("%s::%s" % (r, f) for _, r, f in field)
            else:
                # "m_x+8": the pin points inside the member (a vector's data pointer)
                name, _, addend = field.partition("+")
                upstream = _field_offset(ir, group, rec_name, name, key) + int(addend or 0)
                shown = "%s::%s" % (rec_name, field)
            game = probes.resolve(mod, entry)
            ok = game == upstream
            ent = probes.mods[mod]["entries"][entry]
            if ok and "value" in ent and not ent.get("locator"):
                # a literal offset is a number the DB records but no binary backs (bridge-verified
                # live), so equality is a SKIP (LITERAL) like the literal consts; only a mismatch FAILs
                rows.append(("SKIP", tag, str(upstream), str(game),
                             "LITERAL: %s sigs offset, not measured in any binary; equals the generated %s (%s)"
                             % (mod, shown, why)))
                continue
            rows.append(("PASS" if ok else "FAIL", tag, str(upstream), str(game),
                         "%s: %s" % (shown, why) if ok else
                         "%s is at %d in the generated layout, the binary reads %d: %s" % (shown, upstream, game, why)))
        except (LookupError, ValueError, AssertionError, IndexError, KeyError, StopIteration) as ex:
            rows.append(("FAIL", tag, "-", "-", "unresolved: %r" % (ex,)))
    for _plat, mod, entry, cls, base, why in BASE_OFFSET_PAIRS:
        if _plat != platform:
            continue
        tag = "off:%s/%s" % (mod, entry)
        try:
            vt = ir_vtable_classes(ir)[cls][1]
            b = next(b for b in vt["secondary_bases"] if b["name"] == base)
            upstream = b["msvc_offset"] if platform == "windows" else b["offset"]
            game = probes.resolve(mod, entry)
            ok = game == upstream
            rows.append(("PASS" if ok else "FAIL", tag, str(upstream), str(game),
                         "%s base %s: %s" % (cls, base, why) if ok else
                         "%s's base %s is at %d in the generated layout, the binary says %d: %s" % (cls, base, upstream, game, why)))
        except (LookupError, ValueError, AssertionError, IndexError, KeyError, StopIteration) as ex:
            rows.append(("FAIL", tag, "-", "-", "unresolved: %r" % (ex,)))


def check_identity_bits(platform, ir, probes, rows):
    """The identity lookup's chunk-bits measurement vs upstream's chunk size:
    `1 << bits == S2_CONST_<cname>`. Linux measures the shift out of the instruction (PASS/FAIL);
    the Windows sigs entry is a literal (no locator), so equality is SKIP (LITERAL) and only a
    mismatch FAILs - the same convention as the other literal consts."""
    consts = {c["name"]: c for c in ir.get("consts", {}).get("consts", [])}
    for plat, mod, entry, cname in IDENTITY_BITS:
        if plat != platform:
            continue
        tag = "id:%s/%s" % (mod, entry)
        const = consts.get(cname)
        upstream = const["msvc"]["value"] if const and const.get("kind") == "int" else None
        if upstream is None:
            rows.append(("FAIL", tag, "-", "-", "no measured const %r in the IR (gen.py and IDENTITY_BITS disagree)" % cname))
            continue
        try:
            ent = probes.mods[mod]["entries"][entry]
            bits = probes.resolve(mod, entry)
        except (LookupError, ValueError, AssertionError, IndexError, KeyError) as ex:
            rows.append(("FAIL", tag, str(upstream), "-", "game side unresolved: %s" % ex))
            continue
        ok = 0 <= bits < 32 and (1 << bits) == upstream
        what = "the chunk bits (index >> %d) %s: 1 << %d = %d vs S2_CONST_%s" % (
            bits, "match" if ok else "DO NOT match", bits, 1 << bits if 0 <= bits < 32 else -1, cname)
        if not ok:
            rows.append(("FAIL", tag, str(upstream), str(bits), what + " - a stale header or a moved lookup"))
        elif ent.get("locator"):
            rows.append(("PASS", tag, str(upstream), str(bits), what + " (read out of the lookup's shift instruction)"))
        else:
            rows.append(("SKIP", tag, str(upstream), str(bits),
                         "LITERAL: %s sigs const, not measured in any binary (no locator); %s" % (mod, what)))


def check_identity(platform, ir, probes, rows):
    """(i) the entity-identity chunk geometry, linux only (the windows run emits no
    rows). The chunk bits and the chunk array resolve and are shown; the stride the
    lookup multiplies by compares `==` with the IR's Itanium sizeof(CEntityIdentity)
    when the DB carries it as an entry (libclient.json CCONST_IDENTITY_STRIDE), and
    SKIPs naming that entry when the DB has none - this script adds no DB entries."""
    if platform != "linux":
        return
    for _plat, mod, entry, why in IDENTITY_ENTRIES:
        tag = "id:%s/%s" % (mod, entry)
        try:
            ent = probes.mods[mod]["entries"][entry]
            value = probes.resolve(mod, entry)
        except (LookupError, ValueError, AssertionError, IndexError, KeyError) as ex:
            rows.append(("FAIL", tag, "-", "-", "game side unresolved: %s" % ex))
            continue
        shown = str(value) if ent.get("kind") == "const" else "%#x" % value
        rows.append(("SKIP", tag, "-", shown, why))
    # the stride: game side = a DB entry (libclient.json CCONST_IDENTITY_STRIDE),
    # upstream side = the IR record's Itanium size (what S2_SIZEOF_CEntityIdentity
    # pins on the MSVC side).
    tag = "id:CEntityIdentity-stride"
    stride = [(mod, name) for mod, doc in sorted(probes.mods.items())
              for name, ent in sorted(doc["entries"].items())
              if ent.get("kind") in ("const", "offset") and "STRIDE" in name]
    _plat, grp, rec_name = IDENTITY_STRIDE
    rec = next((r for r in ir.get(grp, {}).get("records", []) if r.get("name") == rec_name), None)
    upstream = (rec.get("itanium") or {}).get("size") if rec else None
    upstream_s = str(upstream) if upstream is not None else "none"
    if stride:
        if len(stride) != 1:
            rows.append(("FAIL", tag, upstream_s, "-",
                         "pair misconfigure: %d DB entries name a STRIDE" % len(stride)))
            return
        mod, name = stride[0]
        try:
            game = probes.resolve(mod, name)
            ok = game == upstream
            rows.append(("PASS" if ok else "FAIL", tag, upstream_s, str(game),
                         "the DB entry measures the identity lookup's stride (`imul` immediate) == the IR's itanium sizeof(%s)" % rec_name
                         if ok else
                         "the DB-measured stride != the IR's itanium sizeof(%s) - a stale header or a wrong IR" % rec_name))
        except (LookupError, ValueError, AssertionError, IndexError) as ex:
            rows.append(("FAIL", tag, upstream_s, "-", "game side unresolved: %s" % ex))
    else:
        rows.append(("SKIP", tag, upstream_s, "-",
                     "the identity stride is measured in the binary (the `imul 0x70` = 112 in the "
                     "identity lookup) but no DB entry carries it - libclient.json "
                     "CCONST_IDENTITY_STRIDE is the entry that does, so it is missing or renamed; "
                     "upstream=%s is the IR's itanium sizeof(CEntityIdentity) (S2_SIZEOF_CEntityIdentity asserts the MSVC twin)"
                     % upstream_s))


def run_checks(platform, ir, probes, defines=None):
    rows = []
    check_platform_groups(platform, ir, rows)
    check_vtables(platform, ir, probes, rows)
    check_consts(platform, ir, probes, rows)
    check_sums_and_offsets(platform, ir, probes, rows)
    check_identity(platform, ir, probes, rows)
    check_identity_bits(platform, ir, probes, rows)
    check_sdk(platform, probes, rows, sc.sdk_defines() if defines is None else defines)
    return rows


# ---------------------------------------------------------------------------
# reporting


def emit(rows, title):
    print("== sdkcheck.py - %s" % title)
    for status, tag, up, game, why in rows:
        print("%-4s %-52s upstream=%-8s game=%-8s %s" % (status, tag, up, game, why))
    n = {"PASS": 0, "FAIL": 0, "SKIP": 0}
    for r in rows:
        n[r[0]] += 1
    print("== %d checks: %d PASS, %d SKIP, %d FAIL" % (len(rows), n["PASS"], n["SKIP"], n["FAIL"]))
    return 1 if n["FAIL"] else 0


def binaries_missing():
    """the build.sh skip path: is the whole binaries tree absent?"""
    root = sc.BINS / "windows"
    return not root.exists()


def linux_binaries_missing(tree):
    """the linux skip path: is the whole linux tree absent?"""
    return not (Path(tree) / "linux").exists()


# ---------------------------------------------------------------------------
# self-test: perturb in-memory IR copies, assert the checks turn FAIL


def self_test(linux_tree):
    ir = load_ir()
    db = sc.load_db()
    probes = Probes(db)
    base = run_checks("windows", ir, probes)
    problems = []

    def expect_fail(perturb_name, make_rows, want_tags):
        failed = {r[1] for r in make_rows() if r[0] == "FAIL"}
        missing = [t for t in want_tags if t not in failed]
        if missing:
            problems.append("%s: expected FAIL in %s, only %s" % (perturb_name, missing, sorted(failed)))
        else:
            print("self-test: %-44s -> FAIL %s (good)" % (perturb_name, sorted(failed)))

    def perturb_len(grp, cls, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for vt in ir2[grp]["vtables"]:
                if vt["class"] == cls:
                    vt["msvc"]["length"] = new
                    return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no vtable %s" % cls)
        expect_fail("%s msvc.length %s != measured" % (cls, new), go, [want_tag])

    def perturb_slot(grp, cls, method, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for vt in ir2[grp]["vtables"]:
                if vt["class"] == cls:
                    for m in vt["methods"]:
                        if m["name"] == method:
                            m["msvc_slot"] = new
                            return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no slot for %s" % cls)
        expect_fail("%s.%s msvc_slot -> %d" % (cls, method, new), go, [want_tag])

    def perturb_field(grp, rec_name, field, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for r in ir2[grp]["records"]:
                if r["name"] == rec_name:
                    for f in r["msvc"]["fields"]:
                        if f["name"] == field:
                            f["offset"] = new
                            return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no field %s::%s" % (rec_name, field))
        expect_fail("%s.%s msvc offset -> %#x" % (rec_name, field, new), go, [want_tag])

    def perturb_base(grp, cls, base_name, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for vt in ir2[grp]["vtables"]:
                if vt["class"] == cls:
                    for b in vt["secondary_bases"]:
                        if b["name"] == base_name:
                            b["msvc_offset"] = new
                            return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no base %s of %s" % (base_name, cls))
        expect_fail("%s base %s msvc offset -> %d" % (cls, base_name, new), go, [want_tag])

    def perturb_record_base(grp, rec_name, base_name, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for r in ir2[grp]["records"]:
                if r["name"] == rec_name:
                    for b in r["bases"]:
                        if b["name"] == base_name:
                            b["offset"] = new
                            return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no record base %s of %s" % (base_name, rec_name))
        expect_fail("%s record base %s offset -> %d" % (rec_name, base_name, new), go, [want_tag])

    def perturb_const(name_, new, want_tag=""):
        def go():
            ir2 = copy.deepcopy(ir)
            for c in ir2["consts"]["consts"]:
                if c["name"] == name_:
                    c["msvc"]["value"] = new
                    return run_checks("windows", ir2, probes)
            raise RuntimeError("perturbation: no const %s" % name_)
        expect_fail("consts.%s msvc value +1" % name_, go, [want_tag])

    if any(r[0] == "FAIL" for r in base):
        problems.append("the unmodified run already FAILs; perturbation evidence would be meaningless")

    perturbations = [
        (perturb_len, ("engine", "ISource2ServerConfig", 28),
         "vt:ISource2ServerConfig@server.dll/GSV_GAMECONFIG_SERVER_VT"),      # == row: upstream shrank
        (perturb_len, ("cvar", "ICvar", 47), "vt:ICvar@tier0.dll/T0V_CCVAR_VT"),      # >= row: upstream > game
        (perturb_len, ("entity", "CEntityInstance", 239),
         "vt:CEntityInstance@server.dll/CBaseEntity"),                        # >= row: past the derived table
        (perturb_len, ("soundsystem", "ISoundOpSystem", 102),
         "vt:ISoundOpSystem@soundsystem.dll/SNV_SOUNDOPSYSTEM_VT"),             # ==+2 row: upstream grew
        (perturb_len, ("soundsystem", "ISoundOpSystem", 100),
         "const:soundsystem.dll/SNC_SOS_VT_SLOTS"),                            # sum row: upstream shrank
        (perturb_const, ("TOKEN_OUTPUT_COLOR", 226443305),
         "const:engine2.dll/S2_ATTR_TOKEN_COLOR"),
        (perturb_len, ("soundsystem", "ISoundEventManager", 64),
         "vt:ISoundEventManager@soundsystem.dll/SNV_SOUNDEVENTMGR_VT"),         # == row: upstream grew
        (perturb_len, ("engine", "IEngineService", 24),
         "vt:IEngineService@engine2.dll/IEngineService"),                      # == row: the destructor slot doubled
        (perturb_len, ("engine", "IRenderService", 40),
         "vt:IRenderService@engine2.dll/EV_RENDERSERVICE_VT"),                 # == row: upstream grew
        (perturb_len, ("engine", "IInputService", 62),
         "vt:IInputService@engine2.dll/EV_INPUTSERVICE_VT"),                   # == row: upstream shrank
        (perturb_len, ("engine", "IVEngineClient2", 186),
         "vt:IVEngineClient2@engine2.dll/EV_ENGINECLIENT_VT"),
        (perturb_len, ("particles", "IParticleSystemMgr", 62),
         "vt:IParticleSystemMgr@particles.dll/CParticleSystemMgr"),            # == row: upstream grew
        (perturb_len, ("particles", "IParticleFunctionDefinition", 4),
         "vt:IParticleFunctionDefinition@particles.dll/CParticleFunctionDefinitionTyped<C_OP_ConstrainDistance>"),
        (perturb_len, ("rendersystem", "IRenderDevice", 256),
         "vt:IRenderDevice@rendersystemvulkan.dll/RV_RENDERDEVICE_VT"),        # ==+37 row: upstream grew
        (perturb_const, ("MAX_ENTITIES_IN_LIST", 1024),
         "id:client.dll/CCONST_IDENTITY_CHUNK_BITS"),                          # 1 << 9 != 1024: the chunk-bits row
        (perturb_len, ("client", "CClientInput", 34),
         "vt:CClientInput@client.dll/CClientInput"),                           # == row: upstream grew
        (perturb_len, ("client", "CClientInput", 34),
         "const:client.dll/CVC_INPUT_VT_SLOTS"),                               # sum row (CClientInput + 0)
        (perturb_len, ("toolutils2", "IUndoable", 13),
         "vt:IUndoable@pet.dll/CBaseUndoable"),                                # ==+N row: upstream grew
        (perturb_len, ("toolutils2", "IBindTarget", 8),
         "vt:IBindTarget@pet.dll/CBaseEditorApp::IBindTarget"),                # secondary COL row
        (perturb_len, ("modeldoc_editor", "IAssetFingerprintListener", 2),
         "vt:IAssetFingerprintListener@modeldoc_editor.dll/CModelDocEditorSession::IAssetFingerprintListener"),
        (perturb_len, ("datamodel", "IDataModel", 141),
         "vt:IDataModel@hammer.dll/HV_DATAMODEL_VT"),                          # ==+1 row: upstream grew
        (perturb_len, ("mapdoclib", "CMapNode", 253),
         "vt:CMapNode@hammer.dll/CMapNode"),                             # == row: upstream grew
        (perturb_len, ("hammer", "IAssetFileListener", 2),
         "vt:IAssetFileListener@hammer.dll/CHammerEditorSession::IAssetFileListener"),
        (perturb_len, ("mapdoclib", "CMapAtom", 18),
         "vt:CMapAtom@hammer.dll/CMapAtom"),                                   # exact row on the class's own RTTI table
        (perturb_base, ("mapdoclib", "CMapNode", "IToolRenderCallbacks", 0xD8),
         "base:CMapGroup/IToolRenderCallbacks@hammer.dll/CMapGroup::IToolRenderCallbacks"),   # generic COL base rule, via DERIVED_AS
        (perturb_base, ("toolutils2", "CBaseEditorApp", "IBindTarget", 0x38),
         "base:CBaseEditorApp/IBindTarget@pet.dll/CBaseEditorApp::IBindTarget"),              # generic COL base rule, a pet row
        (perturb_record_base, ("mapdoclib", "CMapDoc", "IUndoDoc", 0x10),
         "base:CMapDoc/IUndoDoc@hammer.dll/CMapDoc::col_8"),                   # generic COL base rule, a record-only class
    ]
    for fn, argv, want_tag in perturbations:
        fn(*argv, want_tag=want_tag)

    problems += self_test_platforms(ir, probes, base)
    problems += self_test_sdk(ir, probes)

    if problems:
        for p in problems:
            print("self-test: FAIL %s" % p)
        print("self-test: %d problem(s)" % len(problems))
        return 1
    print("self-test: %d perturbations, each turned its check into a FAIL; unmodified run PASSes" % len(perturbations))
    return self_test_linux(linux_tree)


SDK_SELFTEST = [   # (module, pin, the row it must give with the committed header)
    ("scenesystem.dll", "SVS_SS_BEGIN", "PASS"),                 # vslot: the locator's slot index
    ("soundsystem.dll", "SNVS_SOS_START_WITH_PARAMS", "PASS"),   # reversed MSVC overload group
    ("hammer.dll", "HOFF_NODE_CHILD_HANDLES", "PASS"),           # "+8" offset
    ("hammer.dll", "HOFF_DOC_PATH", "PASS"),                     # sum of two generated offsets
    ("pet.dll", "PETOFF_PMGR_FUNC_GET", "PASS"),                 # slot * 8 byte displacement
    ("rendersystemvulkan.dll", "RSC_IMAGE_FMT_DEPTH42", "SKIP"), # literal (no locator)
]


def self_test_sdk(ir, probes):
    """(d) `sdk` rows on an in-memory DB copy: every SDK_SELFTEST entry must carry `sdk` (written by
    sdkadopt.py). With the committed header each
    has its expected status; for EVERY entry carrying `sdk` a header value one off turns its row
    into a FAIL (the sdk rows replaced the IR-perturbed slot:/off: rows of the pins they took
    over: the IR -> header step is gen.py --check, the header -> binary step is this row); an
    entry naming the wrong overload FAILs. Returns the problems."""
    problems = []
    db2 = copy.deepcopy(probes.db)
    p2 = Probes(db2)
    p2.cache = probes.cache
    defines = dict(sc.sdk_defines())
    for mod, pin, _want in SDK_SELFTEST:
        ent = next(e for n, e in p2.mods[mod]["entries"].items() if (e.get("bridge") or n) == pin)
        if not ent.get("sdk"):
            problems.append("sdk:%s/%s: SDK_SELFTEST names an entry without an `sdk` field" % (mod, pin))

    def sdk_rows(defs):
        rows = []
        check_sdk("windows", p2, rows, defs)
        return {r[1].split("/", 1)[1]: r for r in rows}

    rows = sdk_rows(defines)
    for mod, pin, want in SDK_SELFTEST:
        got = rows.get(pin, ("missing",))[0]
        if got != want:
            problems.append("sdk:%s/%s: expected %s with the committed header, got %s" % (mod, pin, want, got))
    adopted = [(mod, n, ent["sdk"]) for mod, doc in p2.mods.items()   # rows are tagged by entry name
               for n, ent in doc["entries"].items() if ent.get("sdk")]
    good = 0
    for mod, pin, expr in adopted:
        mac = next(m for m, _f, _l in sc.sdk_terms(expr) if m)
        bad = dict(defines)
        bad[mac] += 1
        got = sdk_rows(bad).get(pin, ("missing",))[0]
        if got != "FAIL":
            problems.append("sdk:%s/%s: %s + 1 should FAIL, got %s" % (mod, pin, mac, got))
        else:
            good += 1
    print("self-test: %-44s -> FAIL on each of %d sdk rows (good)" % ("header macro + 1", good))
    ent = next(e for e in p2.mods["soundsystem.dll"]["entries"].values()
               if e.get("bridge") == "SNVS_SOS_START_WITH_PARAMS")
    ent["sdk"] = "S2_SLOT_ISoundOpSystem_StartSoundEvent_1"     # the overload a reader would pick by eye
    got = sdk_rows(defines).get("SNVS_SOS_START_WITH_PARAMS", ("missing",))[0]
    if got != "FAIL":
        problems.append("sdk: StartSoundEvent_1 on SNVS_SOS_START_WITH_PARAMS should FAIL, got %s" % got)
    else:
        print("self-test: %-44s -> FAIL (good)" % "sdk names the wrong overload")
    return problems


def self_test_platforms(ir, probes, base):
    """Windows-only groups and the concrete-class `==` row. Returns the problems.

    1. mark the cvar group Windows-only in an in-memory IR copy: the windows rows
       must not change at all; the linux rows (no Linux tree needed - nothing is
       read from a binary) must SKIP its group, its ICvar vtable and its slot pins
       with the Windows-only reason, never PASS or FAIL them;
    2. a concrete-class row: a `==` pair for ICvar against CCvar's own RTTI table
       (the class that implements it) - an IR length equal to the measured table
       PASSes, one slot off FAILs, so the row works in both directions."""
    problems = []
    ir2 = copy.deepcopy(ir)
    ir2["cvar"]["platforms"] = ["windows"]
    if run_checks("windows", ir2, probes) != base:
        problems.append("windows-only marker changed the windows rows")
    lprobes = Probes(sc.load_db(platform="linux"), platform="linux",
                     tree=Path(sc.ROOT) / "target" / "no-such-linux-tree")
    lrows = run_checks("linux", ir2, lprobes)
    by_tag = {r[1]: r for r in lrows}
    for tag in ("group:cvar", "vt:ICvar"):
        row = by_tag.get(tag)
        if not row or row[0] != "SKIP" or "Windows-only" not in row[4]:
            problems.append("linux: %s should SKIP as Windows-only, got %s" % (tag, row))
    ir_cvar_slots = [m for vt in ir2["cvar"]["vtables"] for m in vt["methods"]]
    if not ir_cvar_slots:
        problems.append("perturbation: cvar has no vtable methods")
    bad = [r for r in lrows if "Windows-only" in r[4] and r[0] != "SKIP"]
    if bad:
        problems.append("linux: Windows-only rows that are not SKIP: %s" % bad)
    unmarked = [r for r in run_checks("linux", ir, lprobes) if r[1] in ("group:cvar",)]
    if unmarked:
        problems.append("linux: an unmarked group got a group row")
    if not problems:
        print("self-test: %-44s -> linux SKIPs %s, windows rows unchanged (good)"
              % ("cvar marked Windows-only", sorted(t for t, r in by_tag.items() if "Windows-only" in r[4])))

    global VT_PAIRS
    saved = VT_PAIRS
    pair = (PLATFORM, "cvar", "ICvar", "tier0.dll", "T0V_CCVAR_VT", "==",
            "self-test: concrete-class row, CCvar's own RTTI table vs the interface's msvc length")
    tag = "vt:ICvar@tier0.dll/T0V_CCVAR_VT"
    try:
        VT_PAIRS = [pair]
        rows = []
        check_vtables("windows", ir, probes, rows)
        row = next((r for r in rows if r[1] == tag), None)
        if row is None or not row[3].isdigit():
            problems.append("concrete == row: no measured length (%s)" % (row,))
        else:
            game = int(row[3])
            for length, want in ((game, "PASS"), (game - 1, "FAIL"), (game + 1, "FAIL")):
                ir3 = copy.deepcopy(ir)
                for vt in ir3["cvar"]["vtables"]:
                    if vt["class"] == "ICvar":
                        vt["msvc"]["length"] = length
                rows = []
                check_vtables("windows", ir3, probes, rows)
                got = next(r[0] for r in rows if r[1] == tag)
                if got != want:
                    problems.append("concrete == row with IR length %d (game %d): %s, want %s"
                                    % (length, game, got, want))
            if not problems:
                print("self-test: %-44s -> == row PASS at %d, FAIL at %d and %d (good)"
                      % ("concrete-class == row", game, game - 1, game + 1))
    finally:
        VT_PAIRS = saved
    return problems


def self_test_linux(linux_tree):
    """the linux half, same contract: a doctored in-memory copy must turn its check
    FAIL, never SKIP. Six perturbations (the last two are the soundsystem rows):
      1. a stale upstream header, simulated on an IR copy: an IR vtable for
         IKeyValuesDumpContextAsText with 2 of the 5 slots the game's table has -
         the DB-measured `==` row must FAIL (5 != 2);
      2. a doctored in-memory copy of the linux DB: the located chunk-bits const's
         sig stops matching (what a game change does) - the identity row must FAIL;
      3. same for the located identity stride (id:CEntityIdentity-stride): its
         sig stops matching - the stride row must FAIL instead of SKIPping;
      4. same for a measured VTLEN: the locator's class gone from the module - FAIL;
      5. ISoundEventManager grows a virtual in the IR copy (64): the measured-63 `==` row and
         the 166 sum row must FAIL;
      6. the params struct's hash/entindex offsets move: the two `off:` rows must FAIL;
      7. IRenderService, IInputService, IVEngineClient2, IRenderDevice, IParticleSystemMgr and CClientInput each grow a virtual
         in the IR copy: their libengine2 / librendersystemvulkan / libparticles `==` / `==+N` rows must FAIL.
    """
    if linux_binaries_missing(linux_tree):
        print("self-test: linux skipped - %s is not staged (sigs/README.md 'Linux binaries')" % linux_tree)
        return 0
    ir = load_ir()
    db = sc.load_db(platform="linux")
    probes = Probes(db, platform="linux", tree=linux_tree)
    problems = []
    base = run_checks("linux", ir, probes)
    if any(r[0] == "FAIL" for r in base):
        problems.append("linux: the unmodified run already FAILs; perturbation evidence would be meaningless")

    VTLEN_ROW = "vtlen:libtier0.so/VTLEN_IKeyValuesDumpContextAsText"
    SEM_ROW = "vt:ISoundEventManager@libsoundsystem.so/VTLEN_ISoundEventManager"
    ran = []

    def expect_fail(perturb_name, make_rows, want_tags):
        failed = {r[1] for r in make_rows() if r[0] == "FAIL"}
        missing = [t for t in want_tags if t not in failed]
        if missing:
            problems.append("%s: expected FAIL in %s, only %s" % (perturb_name, missing, sorted(failed)))
        else:
            ran.append(perturb_name)
            print("self-test: %-44s -> FAIL %s (good)" % (perturb_name, sorted(failed)))

    def stale_upstream():
        ir2 = copy.deepcopy(ir)
        ir2["tier0"]["vtables"].append({
            "class": "IKeyValuesDumpContextAsText", "primary_base": None, "secondary_bases": [],
            "methods": [
                {"name": "KvBeginKey", "is_dtor": False, "overrides": None, "itanium_slot": 0},
                {"name": "KvWriteValue", "is_dtor": False, "overrides": None, "itanium_slot": 1},
            ]})
        return run_checks("linux", ir2, probes)

    expect_fail("stale upstream: synthetic IR vtable (2 slots)", stale_upstream, [VTLEN_ROW])

    def break_db(module, entry, do, perturb_name, want_tags):
        def go():
            db2 = copy.deepcopy(db)
            for d in db2.values():
                if d.get("module") == module:
                    do(d["entries"][entry])
            return run_checks("linux", ir, Probes(db2, platform="linux", tree=linux_tree))
        expect_fail(perturb_name, go, want_tags)

    def grow_eventmgr():
        # upstream grew ISoundEventManager by a virtual: the interface length 64 no
        # longer matches the measured secondary table (63) nor the 166 sum
        ir2 = copy.deepcopy(ir)
        for vt in ir2["soundsystem"]["vtables"]:
            if vt["class"] == "ISoundEventManager":
                vt["methods"].append({"name": "Extra", "is_dtor": False, "overrides": None,
                                      "itanium_slot": max(m["itanium_slot"] for m in vt["methods"]) + 1})
        return run_checks("linux", ir2, probes)

    expect_fail("ISoundEventManager grows to 64 slots", grow_eventmgr,
                [SEM_ROW, "const:libsoundsystem.so/SNC_SOS_VT_SLOTS"])

    def grow(group, cls):
        def go():
            ir2 = copy.deepcopy(ir)
            for vt in ir2[group]["vtables"]:
                if vt["class"] == cls:
                    vt["methods"].append({"name": "Extra", "is_dtor": False, "overrides": None,
                                          "itanium_slot": max(m["itanium_slot"] for m in vt["methods"]) + 1})
            return run_checks("linux", ir2, probes)
        return go

    expect_fail("IRenderService grows a virtual", grow("engine", "IRenderService"),
                ["vt:IRenderService@libengine2.so/VTLEN_CRenderService"])
    expect_fail("IInputService grows a virtual", grow("engine", "IInputService"),
                ["vt:IInputService@libengine2.so/VTLEN_CInputService"])
    expect_fail("IVEngineClient2 grows a virtual", grow("engine", "IVEngineClient2"),
                ["vt:IVEngineClient2@libengine2.so/VTLEN_CEngineClient"])
    expect_fail("CClientInput grows a virtual", grow("client", "CClientInput"),
                ["vt:CClientInput@libclient.so/VTLEN_CClientInput", "const:libclient.so/VTLEN_CCSGOInput"])
    expect_fail("IParticleSystemMgr grows a virtual", grow("particles", "IParticleSystemMgr"),
                ["vt:IParticleSystemMgr@libparticles.so/VTLEN_CParticleSystemMgr"])
    expect_fail("IRenderDevice grows a virtual", grow("rendersystem", "IRenderDevice"),
                ["vt:IRenderDevice@librendersystemvulkan.so/VTLEN_CRenderDeviceVulkan"])

    def move_hash():
        ir2 = copy.deepcopy(ir)
        for r in ir2["soundsystem"]["records"]:
            if r["name"] == "SosStartSoundEventParams_t":
                for f in r["fields"] + r["itanium"]["fields"]:
                    if f["name"] in ("m_nSoundEventHash", "m_SourceEntity"):
                        f["offset"] += 4
        return run_checks("linux", ir2, probes)

    expect_fail("SosStartSoundEventParams_t offsets move by 4", move_hash,
                ["off:libsoundsystem.so/SNOFF_SEP_HASH", "off:libsoundsystem.so/SNOFF_SEP_ENTINDEX"])

    break_db("libclient.so", "CCONST_IDENTITY_CHUNK_BITS",
             lambda ent: ent["locator"].__setitem__("sig", ent["locator"]["sig"].replace("49 C1", "48 C1", 1)),
             "CCONST_IDENTITY_CHUNK_BITS sig stops matching",
             ["id:libclient.so/CCONST_IDENTITY_CHUNK_BITS"])
    break_db("libclient.so", "CCONST_IDENTITY_STRIDE",
             lambda ent: ent["locator"].__setitem__(
                 "sig", ent["locator"]["sig"].replace(" C0 70 ", " C0 71 ", 1)),
             "CCONST_IDENTITY_STRIDE sig stops matching",
             ["id:CEntityIdentity-stride"])
    break_db("libtier0.so", "VTLEN_IKeyValuesDumpContextAsText",
             lambda ent: ent["locator"].__setitem__("class", "NoSuchClassAnywhere"),
             "VTLEN_IKeyValuesDumpContextAsText symbol gone",
             [VTLEN_ROW])

    if problems:
        for p in problems:
            print("self-test: FAIL %s" % p)
        print("self-test: linux: %d problem(s)" % len(problems))
        return 1
    print("self-test: linux: %d perturbations, each turned its check into a FAIL; unmodified run has no FAIL"
          % len(ran))
    return 0


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--platform", choices=("windows", "linux"), default="windows")
    ap.add_argument("--tree", type=Path,
                    help="binaries root for --platform linux (default $CS2_LINUX_BINS or target/game-bins-linux-2000930)")
    ap.add_argument("--self-test", action="store_true", help="perturb in-memory IR copies and assert FAILs")
    args = ap.parse_args()

    if args.self_test:
        return self_test(linux_bins_root(args.tree))

    ir = load_ir()
    if args.platform == "linux":
        tree = linux_bins_root(args.tree)
        db = sc.load_db(platform="linux")
        probes = Probes(db, platform="linux", tree=tree)
        if linux_binaries_missing(tree):
            # the build.sh skip path, linux flavour: no linux tree at all (a partial
            # tree says so per module below, fail closed)
            print("sdkcheck.py: SKIP %s is missing - the linux entries need the Linux build"
                  " staged under linux/ (sigs/README.md 'Linux binaries', stagebins.py --linux)" % tree)
            return 0
        builds = sorted({b for d in db.values() for b in d.get("builds", {})} or ["?"])
        title = "s2sdk dual-ABI IR vs Linux (itanium) binaries (--tree %s; DB builds %s - a different install from the Windows build %s)" % (
            tree, ", ".join(builds), sc.CURRENT_BUILD)
        rows = run_checks("linux", ir, probes)
        return emit(rows, title)

    db = sc.load_db()
    probes = Probes(db)

    if binaries_missing():
        # the build.sh skip path: no tree at all (a partial/hidden build would
        # say so per module below instead)
        print("sdkcheck.py: SKIP target/game-bins is missing - the SDK numbers are only"
              " proven against a build after stagebins.py fills the tree (sigs/README.md)")
        return 0

    title = "s2sdk dual-ABI IR vs %s binaries" % ("Windows (MSVC)" if args.platform == "windows" else "Linux (itanium; mode hook, no entries yet)")
    title += " ($CS2_BINS=%s, DB CURRENT_BUILD=%s)" % (sc.BINS, sc.CURRENT_BUILD)
    rows = run_checks(args.platform, ir, probes)
    return emit(rows, title)


if __name__ == "__main__":
    sys.exit(main())
