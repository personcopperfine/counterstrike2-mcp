/*
 * s2sdk.h - Source 2 (CS2; layouts verified on build 2000921, field offsets unchanged in 2000930 per pins.py) interfaces and class layouts used by mcp_bridge.
 *
 * Reconstructed from the shipped binaries (IDA: hammer.dll, scenesystem.dll, engine2.dll,
 * rendersystemvulkan.dll, toolframework2.dll; RTTI names given per type). Freestanding C:
 * no CRT/SDK headers. Include after the bridge's u8/u16/u32/u64 typedefs and, in this order,
 * bridge_pins.h -> s2sdk_gen.h (mcp_bridge.c does).
 *
 * Every slot, member offset, struct size and vtable length the SDK headers describe comes from
 * the generated bridge/s2sdk_gen.h (S2_SLOT_/S2_OFF_/S2_SIZEOF_/S2_VTLEN_/S2_CONST_, S2_IFACE_
 * strings, and padded S2_<T> structs for the records gen.py's C_EMIT_STRUCTS lists - the tool
 * classes overlay a typed vtable on them with S2_OBJECT). bridge_pins.h keeps only what the
 * headers cannot give: RVAs, Qt widget-chain offsets, and the few layouts no header declares
 * yet (polymesh, FGD class, editor settings, render attributes). A new slot or offset is a
 * header declaration first (a commit on crates/s2sdk/upstream, regenerate) and a sigs entry
 * with `sdk` second, the binary measurement that checks it (bridge/README.md).
 *
 * How objects are reached (most stable first):
 *   1. Interface by name: the app-system factory our tool receives in Connect()
 *      (S2_IFACE_* / S2_TOOLSYSTEM2_VERSION strings), or a module's exported CreateInterface.
 *   2. Exported symbols (GetProcAddress), e.g. hammer.dll's CreateInterface.
 *   3. Virtual methods: typed vtable structs below, indexed by the generated S2_SLOT_ numbers;
 *      pins.py still verifies each sigs-measured slot's target at load time.
 *   4. Class fields: generated S2_<T> structs, or S2_OFF_ offsets asserted with S2_FIELD.
 *   5. Non-virtual, non-exported functions (no interface exists): HammerFns / SceneFns /
 *      EngineFns tables of typed pointers, bound once from RVAs that pins.py re-finds with the
 *      anchors in anchors.json (signatures / call sites / string refs), after the module's
 *      identity check passed (byte-pinned prologues, fail closed).
 *
 * Calling convention: MSVC x64; `this` is the first argument.
 */
#ifndef S2SDK_H
#define S2SDK_H

/* (c): S2_SLOT/S2_FIELD compile the claim into the TU - a struct shape that drifts from
   bridge_pins.h / s2sdk_gen.h fails the build instead of calling into the wrong place. */
#define S2_OFFSETOF(t, m) __builtin_offsetof(t, m)
#define S2_SLOT(vt, m, slot) _Static_assert(S2_OFFSETOF(vt, m) == (slot) * 8, #vt "::" #m " slot")
#define S2_FIELD(t, m, off) _Static_assert(S2_OFFSETOF(t, m) == (off), #t "::" #m " offset")
#define S2_PAD(a, b) void *_pad_##a##_##b[(b) - (a) + 1]     /* vtable slots a..b inclusive */

/* (c) A game object = its generated whole-object layout (s2sdk_gen.h, `S2_<Class>`, every offset
   asserted there) with the typed vtable pointer overlaid on the layout's leading vptr pad. The
   generated members keep the IR names (m_pActiveSession, ...); a member the bridge wants under
   another type goes through a small typed accessor below. The anonymous-member spelling is the
   MSVC extension clang accepts on this target. */
#pragma clang diagnostic ignored "-Wmicrosoft-anon-tag"
#define S2_OBJECT(Class, Layout, VtblType) \
    struct Class { union { Layout; struct { const VtblType *vt; }; }; }; \
    _Static_assert(S2_OFFSETOF(struct Class, vt) == 0, #Class "::vt is the vptr")

/* ---------------- interface version strings (factory / CreateInterface) ----------------
   the five upstream headers name are generated (s2sdk_gen.h: S2_IFACE_TOOLFRAMEWORK2,
   S2_IFACE_SCENESYSTEM, S2_IFACE_SCENEUTILS, S2_IFACE_RENDERSERVICE, S2_IFACE_RENDER_DEVICE) */
#define S2_TOOLSYSTEM2_VERSION    "ToolSystem2_001"     /* (b) hammer.dll CreateInterface -> CHammerApp; no upstream macro */
#define S2_TOOLSYSTEM2_MODELDOC_VERSION "ToolSystem2_ModelDoc"   /* (b) modeldoc_editor.dll CreateInterface -> CModelDocEditorApp; no upstream macro */

/* (c) the Win32 export name / factory ABI signature every module's factory is reached through */
#define S2_SYM_CREATEINTERFACE    "CreateInterface"
typedef void *(*S2CreateInterfaceFn)(const char *name, int *ret);

/* ---------------- basic types ---------------- */
/* (b) CHammerPolyMesh component handle: 16 opaque bytes, no upstream type (S1 adds HOFF_MESHHANDLE_BYTES) */
typedef struct { u8 b[16]; } S2MeshHandle;
_Static_assert(sizeof(S2MeshHandle) == 16, "S2MeshHandle bytes");
/* (b) CUtlString: tier0.h ships it; the bridge only ever needs its single `char *` member
   (S2_OFF_CUtlString_m_pString 0, s2sdk_gen.h). An empty string is m_pString == 0 (review C1:
   slot 26 hands out a CUtlString *; the empty case must be seen at THIS level, not by strcmp). */
typedef struct { const char *m_pString; } CUtlString;
/* (b) CUtlVector<S2MeshHandle> with caller-owned memory (MeshInitFromPolygons/CollectHandles/
   DeleteFaces; caller-owned == never realloc, never free): the generic tier0 CUtlVector layout
   is asserted below against the generated CUtlVector<CEntityHandle>, and the alignment gap at
   +4 is named `pad` so the call sites can zero it. (c) `flags` (+20 == m_nGrowSize) takes the
   generated EXTERNAL_CONST_BUFFER_MARKER (1<<30), not the plain EXTERNAL_BUFFER_MARKER (1<<31);
   S2_UTLVEC_EXTERNAL only spells that value. */
typedef struct { int size; int pad; S2MeshHandle *mem; int alloc; int flags; } S2HandleVec;
#define S2_UTLVEC_EXTERNAL S2_CONST_EXTERNAL_CONST_BUFFER_MARKER
_Static_assert(sizeof(S2HandleVec) == S2_SIZEOF_CUtlVector_CEntityHandle, "S2HandleVec size == CUtlVector layout");
_Static_assert(S2_OFFSETOF(S2HandleVec, size) == S2_OFF_CUtlVector_CEntityHandle_m_Size, "S2HandleVec::size");
_Static_assert(S2_OFFSETOF(S2HandleVec, pad) == 4, "S2HandleVec::pad");
_Static_assert(S2_OFFSETOF(S2HandleVec, mem) == S2_OFF_CUtlVector_CEntityHandle_m_pMemory, "S2HandleVec::mem");
_Static_assert(S2_OFFSETOF(S2HandleVec, alloc) == S2_OFF_CUtlVector_CEntityHandle_m_nAllocationCount, "S2HandleVec::alloc");
_Static_assert(S2_OFFSETOF(S2HandleVec, flags) == S2_OFF_CUtlVector_CEntityHandle_m_nGrowSize, "S2HandleVec::flags");
/* (b) scenesystem scratch-target description (ISceneSystem::AddView); no upstream type */
typedef struct { int w, h; u8 msaa, pad; } S2RenderTargetDesc;
/* (b) scenesystem/rendersystem pixel rect (IRenderDevice::ReadTexturePixels): same layout as upstream Rect_t (tier0/recttypes.h: x, y, width, height) */
typedef struct { int x, y, w, h; } S2Rect;

/* (b) forward declarations: the I* ones upstream forward-declares only, the rest not at all */
typedef struct IToolFramework2 IToolFramework2;
typedef struct ISceneSystem ISceneSystem;
typedef struct ISceneUtils ISceneUtils;
typedef struct ISceneView ISceneView;
typedef struct ISceneWorld ISceneWorld;
typedef struct IRenderingPipeline IRenderingPipeline;
typedef struct IRenderService IRenderService;
typedef struct IRenderDevice IRenderDevice;
typedef struct IDataModel IDataModel;
typedef struct CDmElement CDmElement;
typedef struct CDmAttribute CDmAttribute;
typedef struct CHammerApp CHammerApp;
typedef struct CHammerEditorSession CHammerEditorSession;
typedef struct CMapDocSession CMapDocSession;
typedef struct CMapDoc CMapDoc;
typedef struct CMapRootElement CMapRootElement;
typedef struct CMapDocHistory CMapDocHistory;
typedef struct CSelection CSelection;
typedef struct CMapNode CMapNode;
typedef struct CMapMesh CMapMesh;
typedef struct CHammerPolyMesh CHammerPolyMesh;
typedef struct CMapViewMgr CMapViewMgr;
typedef struct CMapView CMapView;
typedef struct CCamera CCamera;
typedef struct CModelDocEditorApp CModelDocEditorApp;
typedef struct CModelDocEditorSession CModelDocEditorSession;
typedef struct CPetApp CPetApp;
typedef struct CPetSession CPetSession;
typedef struct CPetDoc CPetDoc;
typedef struct CPetRootNode CPetRootNode;

/* ======================================================================================
 * toolframework2.dll - (b) upstream has `class IToolFramework2;` (interfaces.h:97) only,
 * so this body and both slots are sigs pins
 * ====================================================================================== */
typedef struct {
    S2_PAD(0, 21);
    /* GetToolIndexByName(out, name): stores a 1-byte index, 0xFF = not found */
    void *(*GetToolIndexByName)(IToolFramework2 *self, u64 *outIndex, const char *name);
    S2_PAD(23, 23);
    /* LoadToolSystem(idx): loads the tool DLL once per index (cached; never retried after a
       failure), calls tool slot 11 and returns the tool object. A failure pops a modal
       ("Failed to load/initialize tool system"), so the caller runs it at most once per
       session and only for an index GetToolIndexByName confirmed. This is what
       `tool_request pet` does minus tool slot 16 (a nop in pet). */
    void *(*LoadToolSystem)(IToolFramework2 *self, u8 toolIndex);
    S2_PAD(25, 80);
    void *(*GetToolEntryByIndex)(IToolFramework2 *self, u32 index);
} IToolFramework2Vtbl;
struct IToolFramework2 { const IToolFramework2Vtbl *vt; };
S2_SLOT(IToolFramework2Vtbl, GetToolIndexByName, S2_SLOT_IToolFramework2_FindTool);
S2_SLOT(IToolFramework2Vtbl, LoadToolSystem, S2_SLOT_IToolFramework2_LoadTool);
S2_SLOT(IToolFramework2Vtbl, GetToolEntryByIndex, S2_SLOT_IToolFramework2_GetToolSettings);

/* ======================================================================================
 * scenesystem.dll - (b) upstream forward-declares ISceneSystem/ISceneUtils/ISceneView and
 * defines no matching body; the slots are sigs pins
 * ====================================================================================== */
/* CSceneSystem_002 (.?AVCSceneSystem@@; vtable SV_SCENESYSTEM_VT) */
typedef struct {
    S2_PAD(0, 19);
    void (*BeginRenderingViews)(ISceneSystem *self, IRenderDevice *device, u8 flags);            /* 20 */
    ISceneView *(*AddView)(ISceneSystem *self, const char *name, void *viewId, void *frustum,
                              u64 swapChain, ISceneWorld *world, S2_RenderViewport_t *vp,
                              const void *addWorldData, u32 nPriority);                           /* 21: nPriority = 65536 (the default view priority, not flags; a bridge-chosen literal at the call in mcp_bridge.c); addWorldData -> CSceneView::AddWorld (0x208 B), may be 0 */
    S2_PAD(22, 24);
    void (*FinishRenderingViews)(ISceneSystem *self, float flTime);                               /* 25: flTime FLT_MAX = use the current platform time */
    u8 (*WaitForRenderingToComplete)(ISceneSystem *self);                                         /* 26: bool, always 1 */
    S2_PAD(27, 94);
    void *(*CreateViewId)(ISceneSystem *self, void *outId16, u32 id);                          /* 95: writes only the low dword of qword 2; zero outId16 first */
} ISceneSystemVtbl;
struct ISceneSystem { const ISceneSystemVtbl *vt; };
S2_SLOT(ISceneSystemVtbl, BeginRenderingViews, S2_SLOT_ISceneSystem_BeginRenderingViews);
S2_SLOT(ISceneSystemVtbl, AddView, S2_SLOT_ISceneSystem_AddView);
S2_SLOT(ISceneSystemVtbl, FinishRenderingViews, S2_SLOT_ISceneSystem_FinishRenderingViews);
S2_SLOT(ISceneSystemVtbl, WaitForRenderingToComplete, S2_SLOT_ISceneSystem_WaitForRenderingToComplete);
S2_SLOT(ISceneSystemVtbl, CreateViewId, S2_SLOT_ISceneSystem_CreateViewId);

/* CSceneUtils (.?AVCSceneUtils@@) */
typedef struct {
    S2_PAD(0, 25);
    IRenderingPipeline *(*FindRenderingPipeline)(ISceneUtils *self, u32 nameToken);             /* 26 */
} ISceneUtilsVtbl;
struct ISceneUtils { const ISceneUtilsVtbl *vt; };
S2_SLOT(ISceneUtilsVtbl, FindRenderingPipeline, S2_SLOT_ISceneUtils_FindRenderingPipeline);

/* CSceneView, the scene view AddView returns (.?AVCSceneView@@) */
typedef struct {
    S2_PAD(0, 20);
    void (*BeginLayerGroup)(ISceneView *self, const char *name);                                  /* 21 */
    void (*EndLayerGroup)(ISceneView *self);                                                      /* 22 */
    S2_PAD(23, 23);
    void (*AddFlags)(ISceneView *self, u32 flags);                                                /* 24 */
    S2_PAD(25, 43);
    void (*MarkRenderTargetForReadback)(ISceneView *self, long long handle);                      /* 44 */
    void *(*GetRenderTargetTexture)(ISceneView *self, void **outTexture, long long handle);       /* 45 */
    S2_PAD(46, 87);
    u64 (*CreateScratchTargets)(ISceneView *self, int count, const char **names, int *formats,
                                u32 *bindFlags, long long **outHandles, u32 w, u32 h, u8 msaa,
                                u32 flags);                                                       /* 88 */
} ISceneViewVtbl;
struct ISceneView { const ISceneViewVtbl *vt; };
S2_SLOT(ISceneViewVtbl, BeginLayerGroup, S2_SLOT_ISceneView_BeginLayerGroup);
S2_SLOT(ISceneViewVtbl, EndLayerGroup, S2_SLOT_ISceneView_EndLayerGroup);
S2_SLOT(ISceneViewVtbl, AddFlags, S2_SLOT_ISceneView_AddFlags);
S2_SLOT(ISceneViewVtbl, MarkRenderTargetForReadback, S2_SLOT_ISceneView_MarkRenderTargetForReadback);
S2_SLOT(ISceneViewVtbl, GetRenderTargetTexture, S2_SLOT_ISceneView_GetRenderTargetTexture);
S2_SLOT(ISceneViewVtbl, CreateScratchTargets, S2_SLOT_ISceneView_CreateScratchTargets);
/* (b) S2_VIEWFLAG_SIZED_SCRATCH is a sigs const (a measurement, emitted into bridge_pins.h); the TOOLS3D flag (16)
   and the AddView priority (0x10000) are bridge-owned literals at their use site in mcp_bridge.c - not defines here */

/* (b) CRenderingPipelineAlias ("Tools" alias -> CSGO via gameinfo; .?AVCRenderingPipelineAlias@@);
   the "Tools" name token is the generated S2_CONST_TOKEN_TOOLS (MurmurHash2LowerCase) */
typedef struct {
    void (*AddLayersToView)(IRenderingPipeline *self, ISceneView *view, S2_RenderViewport_t *vp,
                            S2RenderTargetDesc *rt, void *attrs);                                 /* 0 */
    S2_PAD(1, 2);
    /* 3: really GetLayerViewport(u32 layerToken, const RenderViewport_t *in, RenderViewport_t *out):
       copies *in to *out (28 B) and adjusts the depth range per layer token (0 / VIEWMODEL / 3DSkyBox).
       out must be a full S2_RenderViewport_t (docs/interfaces/IRenderingPipeline.md slot 3). */
    void (*GetLayerViewport)(IRenderingPipeline *self, u32 layerToken, const S2_RenderViewport_t *in, S2_RenderViewport_t *out); /* 3 */
    S2_PAD(4, 7);
    u32 (*ScratchTargetFlags)(IRenderingPipeline *self);                                          /* 8 */
    u32 (*ColorBindFlags)(IRenderingPipeline *self);                                              /* 9 */
    u32 (*DepthBindFlags)(IRenderingPipeline *self);                                              /* 10 */
} IRenderingPipelineVtbl;
struct IRenderingPipeline { const IRenderingPipelineVtbl *vt; };
S2_SLOT(IRenderingPipelineVtbl, AddLayersToView, S2_SLOT_IRenderingPipeline_AddLayersToView);
S2_SLOT(IRenderingPipelineVtbl, GetLayerViewport, S2_SLOT_IRenderingPipeline_unk103);
S2_SLOT(IRenderingPipelineVtbl, ScratchTargetFlags, S2_SLOT_IRenderingPipeline_unk108);
S2_SLOT(IRenderingPipelineVtbl, ColorBindFlags, S2_SLOT_IRenderingPipeline_unk109);
S2_SLOT(IRenderingPipelineVtbl, DepthBindFlags, S2_SLOT_IRenderingPipeline_unk1010);

/* (b) CFrustum (mathlib, statically linked; no vtable) 0x420 B, fields set before
   UpdateMatrices. Upstream mathlib/camera.h CFrustum is a *different* type (Frustum_t +
   Camera_t, m_flAspect at 356, not 0x420 B), so this copy stays bridge-owned (design R2);
   its offsets are sigs pins, never generator input. */
typedef struct {
    S2_Vector origin;         /* 0x00 */
    S2_QAngle angles;         /* 0x0C */
    float fovX;               /* 0x18 horizontal degrees */
    float aspect;             /* 0x1C */
    float znear, zfar;        /* 0x20 */
    float reserved28[2];      /* 0x28 -1,-1 in Hammer's default template */
    u8 _p0[0x170 - 0x30];
    float ndc[4];             /* 0x170 viewport rect in NDC: -1,-1,1,1 = full */
    u8 _p1[S2_SIZEOF_CFrustum - 0x180];
} CFrustum;
S2_FIELD(CFrustum, fovX, S2_OFF_Camera_t_m_flFOVX);
S2_FIELD(CFrustum, aspect, S2_OFF_Camera_t_m_flAspect);
S2_FIELD(CFrustum, znear, S2_OFF_Camera_t_m_flZNear);
S2_FIELD(CFrustum, ndc, S2_OFF_CFrustum_m_flClipSpaceBottomLeftX);
_Static_assert(sizeof(CFrustum) == S2_SIZEOF_CFrustum, "CFrustum size");

/* ======================================================================================
 * engine2.dll - (b) upstream forward-declares IRenderService only
 * ====================================================================================== */
/* CRenderService (.?AVCRenderService@@) */
typedef struct {
    S2_PAD(0, 28);
    ISceneWorld *(*GetSceneWorld)(IRenderService *self);   /* 29: CRenderingWorldSession scene world (0 = no map) */
} IRenderServiceVtbl;
struct IRenderService { const IRenderServiceVtbl *vt; };
S2_SLOT(IRenderServiceVtbl, GetSceneWorld, S2_SLOT_IRenderService_GetSceneWorld);

/* CRenderAttributes: per-view render attribute block (engine2, 928 bytes; ctor/dtor are non-virtual ->
   EngineFns). Render-target handles live in an 8-byte table at +784 keyed by name token; the
   tokens are the generated S2_CONST_TOKEN_OUTPUT_COLOR / S2_CONST_TOKEN_OUTPUT_DEPTH. */
_Static_assert(S2OFF_ATTRS_HANDLE_TABLE < S2_SIZEOF_CRenderAttributes, "render attribute handle table offset outside the block");
typedef struct {
    u8 _p0[S2OFF_ATTRS_HANDLE_TABLE];
    u8 handleTable[S2_SIZEOF_CRenderAttributes - S2OFF_ATTRS_HANDLE_TABLE];
} S2RenderAttrs;
_Static_assert(sizeof(S2RenderAttrs) == S2_SIZEOF_CRenderAttributes, "attrs size");

/* ======================================================================================
 * rendersystemvulkan.dll - (b) upstream forward-declares IRenderDevice only
 * ====================================================================================== */
/* CRenderDeviceVulkan (.?AVCRenderDeviceVulkan@@) */
typedef struct {
    S2_PAD(0, 73);
    u8 (*ReadTexturePixels)(IRenderDevice *self, void *texture, S2Rect *src, u32 a3, int a4,
                            S2Rect *dst, void *pixels, int format, int pitch);                  /* 74; RGBA8888 maps to an sRGB Vulkan format, so the bytes read back are sRGB-encoded */
    S2_PAD(75, 167);
    u8 (*IsUsingReverseDepth)(IRenderDevice *self);     /* 168: the reverse-depth flag (RenderSystem/UseReverseDepth, byte at this+8). The bridge picks depth
                                                           format 48 when it is nonzero, else 42 (behaviour kept; the pin was first called PreferDepthFormat48) */
} IRenderDeviceVtbl;
struct IRenderDevice { const IRenderDeviceVtbl *vt; };
S2_SLOT(IRenderDeviceVtbl, ReadTexturePixels, S2_SLOT_IRenderDevice_ReadTexturePixels);
S2_SLOT(IRenderDeviceVtbl, IsUsingReverseDepth, S2_SLOT_IRenderDevice_IsUsingReverseDepth);
/* (b) the ImageFormat ReadTexturePixels takes is the sigs const S2_CONST_IMAGE_FORMAT_RGBA8888
   (generated, s2sdk_gen.h), not a define here */

/* ======================================================================================
 * hammer.dll - data model (statically linked; g_pDataModel is hammer-internal)
 * (b) upstream has no datamodel at all
 * ====================================================================================== */
/* CDataModel */
typedef struct {
    S2_PAD(0, 20);
    CDmElement *(*GetElement)(IDataModel *self, u32 handle);                       /* 21 (vfn+168) */
    S2_PAD(22, 52);
    void (*GetSymbol)(IDataModel *self, S2_CUtlSymbolLarge *out, const char *name); /* 53 (vfn+424): intern */
} IDataModelVtbl;
struct IDataModel { const IDataModelVtbl *vt; };
S2_SLOT(IDataModelVtbl, GetElement, S2_SLOT_IDataModel_GetElement);
S2_SLOT(IDataModelVtbl, GetSymbol, S2_SLOT_IDataModel_GetSymbol);

typedef struct {
    S2_PAD(0, 3);
    u8 (*IsA)(CDmElement *self, const S2_CUtlSymbolLarge *typeSymbol);             /* 4 (vfn+32): type symbol, 8 B */
} CDmElementVtbl;

/* CDmAttribute: singly linked list hanging off the element */
struct CDmAttribute {
    CDmAttribute *next;       /* 0x00 */
    void *value;              /* 0x08 pointer to the value storage */
    u8 _p0[0x1C - 0x10];
    u32 typeFlags;            /* 0x1C low 6 bits = attribute type */
    const char *name;         /* 0x20 */
};
S2_FIELD(CDmAttribute, typeFlags, S2_OFF_CDmAttribute_m_nFlags);
S2_FIELD(CDmAttribute, name, S2_OFF_CDmAttribute_m_Name);
/* (b) DMAttributeType_t values the element inspector applies: mask = low 6 bits, then the four
   types build_keyvalues writes. No upstream enum (S1 adds them as sigs consts; until then
   these are literals read out of hammer.dll). */
#define S2_ATTR_TYPE_MASK 0x3Fu
enum { S2_AT_INT = 2, S2_AT_FLOAT = 3, S2_AT_BOOL = 4, S2_AT_STRING = 5 };

struct CDmElement {
    const CDmElementVtbl *vt;
    u8 _p0[S2_OFF_CDmElement_m_pAttributes - 8];
    CDmAttribute *attrs;      /* 0x48 */
};
S2_FIELD(CDmElement, attrs, S2_OFF_CDmElement_m_pAttributes);

/* ======================================================================================
 * hammer.dll - application / session / document - (b) none of these types exists upstream
 * ====================================================================================== */
/* CHammerApp: ToolSystem2_001 (hammer CreateInterface), lazy singleton g_CHammerApp */
typedef struct {
    S2_PAD(0, 17);
    /* OpenFile returns the session (0 on failure / the re-entrancy guard app+0x1A3); flags 1 is
       what the UI passes. Modals on a bad path (missing file, outside the content dir, load
       errors), so map.open pre-checks the host file and the open-session vector first. Inherited
       from CBaseEditorApp (slot 18 of both vtables is the same function). */
    void *(*OpenFile)(CHammerApp *self, const char *path, u32 flags);                            /* 18 */
} CHammerAppVtbl;

S2_OBJECT(CHammerApp, S2_CBaseEditorApp, CHammerAppVtbl);   /* the session list and active session are CBaseEditorApp's */
static inline CHammerEditorSession *CHammerApp_ActiveSession(const CHammerApp *a) { return (CHammerEditorSession *)a->m_pActiveSession; }
static inline u32 CHammerApp_SessionCount(const CHammerApp *a) { return (u32)a->m_Sessions.m_Size; }
/* the vector OpenFileInternal walks */
static inline CHammerEditorSession **CHammerApp_Sessions(const CHammerApp *a) { return (CHammerEditorSession **)a->m_Sessions.m_pMemory; }
S2_SLOT(CHammerAppVtbl, OpenFile, S2_SLOT_CBaseEditorApp_OpenFile);

/* CHammerEditorSession (.?AVCHammerEditorSession@@) */
typedef struct {
    S2_PAD(0, 6);
    /* ReloadFromDisk: re-reads the map through the document manager (no prompt; unsaved in-editor
       edits are discarded). `void`: the body tail-jumps into the title/view refresh, whose epilogue
       never writes rax, so no success value exists - do not read one (review M1,
       protocol-diff-m2.16.md). */
    void (*ReloadFromDisk)(CHammerEditorSession *self);                                           /* 7 */
    S2_PAD(8, 17);
    /* 18: bool Save( const char *oldPath, const char *newPath, bool, bool ) - the last two are bytes
       (movzx r9b / the stack byte) and the return is only defined in al (re12 section 7). The bridge
       passes (path, path, 0, 0); a non-zero third argument skips the source-control prompt and the
       layout save. */
    u8 (*Save)(CHammerEditorSession *self, const char *oldPath, const char *newPath,
               u8 arg3, u8 arg4);                                                                 /* 18 */
    S2_PAD(19, 25);
    /* REVIEW C1: slot 26 returns the ADDRESS of the docSession's CUtlString member
       (`lea rax,[rcx+0x6B0]` @hammer!0x13ED70, with a static default CUtlString when there is no
       doc) - never read a `const char *` out of it (the bytes of a char* pointer are not a
       string). The doc's path TEXT is `CMapDocSession_Path(docSession)` (the `char *` at +0x6B0),
       and the readers that need it are `map_session_path` (bridge/mcp_bridge_map.inc, which also
       probes readability). The slot itself stays pinned here; the bridge binds no address to it. */
    const CUtlString *(*GetMapFilePath)(CHammerEditorSession *self);                             /* 26 */
} CHammerEditorSessionVtbl;
S2_OBJECT(CHammerEditorSession, S2_CHammerEditorSession, CHammerEditorSessionVtbl);
S2_SLOT(CHammerEditorSessionVtbl, ReloadFromDisk, S2_SLOT_CBaseEditorSession_ReloadDocument);
S2_SLOT(CHammerEditorSessionVtbl, Save, S2_SLOT_CBaseEditorSession_Save);
S2_SLOT(CHammerEditorSessionVtbl, GetMapFilePath, S2_SLOT_CHammerEditorSession_GetMapFilePath);
/* REVIEW C1: the session's map path TEXT is the doc's `char *` at +0x6B0 (CMapDocSession_Path
   below), read from the doc slot 26 points at - slot 26 itself hands back the ADDRESS of that
   CUtlString member (lea rax,[rcx+0x6B0] @0x13ED70; the static default CUtlString it substitutes
   for no-doc is the empty string). Readers use CMapDocSession_Path + their own
   readability probe; the bridge's map.open helper (mcp_bridge_map.inc) is
   `map_session_path`. */

/* CMapDoc (.?AVCMapDoc@@) - embedded in CMapDocSession at +0x10; its vtable is not typed here */
S2_OBJECT(CMapDoc, S2_CMapDoc, void);
/* (the edit-scope stack's depth is the count that leads its CUtlVector<void *> m_Scopes) */
static inline u32 CMapDoc_ScopeDepth(const CMapDoc *d) { return (u32)d->m_EditScopeStack.m_Scopes.m_Size; }

/* CMapDocSession (.?AVCMapDocSession@@): the CMapDoc lives INSIDE it (m_Doc, by value) */
S2_OBJECT(CMapDocSession, S2_CMapDocSession, void);
static inline CMapDoc *CMapDocSession_Doc(CMapDocSession *s) { return (CMapDoc *)&s->m_Doc; }
static inline CMapDocSession *CMapDoc_Session(CMapDoc *d) { return (CMapDocSession *)((u8 *)d - S2_OFF_CMapDocSession_m_Doc); }
/* the doc's path TEXT (the `char *` in the CUtlString m_Path), reached through the session */
static inline char *CMapDocSession_Path(CMapDocSession *s) { return (char *)s->m_Doc.m_Path.m_pString; }
static inline CHammerEditorSession *CMapDocSession_Owner(CMapDocSession *s) { return s->m_pOwner; }

/* CMapRootElement (.?AVCMapRootElement@@): DME root of the document; m_fileId is the CDmElement
   DmFileId_t and m_hWorld the world element handle (CMapDoc::SetMapRoot: GetElement -> doc->world) */
S2_OBJECT(CMapRootElement, S2_CMapRootElement, void);

/* CMapDocHistory (.?AVCMapDocHistory@@) */
typedef struct {
    S2_PAD(0, 1);
    void (*Undo)(CMapDocHistory *self);                       /* 2 */
    u8 (*HasEntries)(CMapDocHistory *self);                   /* 3 */
} CMapDocHistoryVtbl;
typedef S2_UndoGroup_t CMapUndoGroup;     /* m_nSerial, m_szName (the undo label) */
S2_OBJECT(CMapDocHistory, S2_CMapDocHistory, CMapDocHistoryVtbl);
/* the group stack is CUndoHistory's CUtlVector<UndoGroup_t *> (the IR names it m_unk001) */
static inline u32 CMapDocHistory_Count(const CMapDocHistory *h) { return (u32)h->m_unk001.m_Size; }
static inline CMapUndoGroup **CMapDocHistory_Groups(const CMapDocHistory *h) { return (CMapUndoGroup **)h->m_unk001.m_pMemory; }
S2_SLOT(CMapDocHistoryVtbl, Undo, S2_SLOT_CUndoHistory_Undo);
S2_SLOT(CMapDocHistoryVtbl, HasEntries, S2_SLOT_CUndoHistory_HasEntries);

/* ======================================================================================
 * hammer.dll - nodes and meshes - (b) none of these types exists upstream
 * ====================================================================================== */
/* (b) one CMapEntityIONode connection: the 80-byte record CMapEntityIONode::AddConnection
   (HFN_ADD_CONNECTION) copies into the node's live vector AND the element type that vector holds
   (S2_CMapEntityIONode::m_Connections: m_Size, m_pMemory). Every offset is the
   one re-props.md Q3 read in the binary; only `source` and the gap at 0x48 have no DME attribute.
   The five string pointers are read (strlen + SetDirect) only during the AddConnection call, so
   bridge-owned buffers are fine there - never hand one to a CUtlString destructor. The 16-byte
   paramMap must be a valid initialized KeyValues3 (AddConnection copies it): call
   HFN_KV3_INIT(1, 8) before and HFN_KV3_FREE after, exactly as "Add New Output" does. */
typedef struct { u64 _w[2]; } S2KV3;
_Static_assert(sizeof(S2KV3) == 16, "KeyValues3 value block");
typedef struct {
    const char *source;        /* 0x00 owner targetname (load-filled; no saved attribute) */
    const char *output;        /* 0x08 outputName */
    const char *target;        /* 0x10 targetName */
    const char *input;         /* 0x18 inputName */
    const char *param;         /* 0x20 overrideParam */
    int targetType;            /* 0x28 7 = what the UI and every saved Valve connection use */
    u32 _pad0;                 /* 0x2C */
    S2KV3 paramMap;            /* 0x30 (KV3; must be initialized/copied, see above) */
    float delay;               /* 0x40 */
    int timesToFire;           /* 0x44 -1 = always */
    u8 _pad1[0x4C - 0x48];     /* 0x48 two flag bytes + 0x4A..0x4B (none has a DME attribute) */
    int serial;                /* 0x4C */
} S2ConnectionSrc;
S2_FIELD(S2ConnectionSrc, source, 0x00);
S2_FIELD(S2ConnectionSrc, output, 0x08);
S2_FIELD(S2ConnectionSrc, target, 0x10);
S2_FIELD(S2ConnectionSrc, input, 0x18);
S2_FIELD(S2ConnectionSrc, param, 0x20);
S2_FIELD(S2ConnectionSrc, targetType, 0x28);
S2_FIELD(S2ConnectionSrc, paramMap, 0x30);
S2_FIELD(S2ConnectionSrc, delay, 0x40);
S2_FIELD(S2ConnectionSrc, timesToFire, 0x44);
S2_FIELD(S2ConnectionSrc, serial, 0x4C);
_Static_assert(sizeof(S2ConnectionSrc) == 80, "S2ConnectionSrc is the 80-byte vector element");

typedef struct {
    S2_PAD(0, 3);
    u8 (*IsA)(CMapNode *self, const S2_CUtlSymbolLarge *typeSymbol);         /* 4: CDmElement::IsA */
    S2_PAD(5, 16);
    /* 17/18/19 are the transform setters (re-props.md Q2: the base stores are +0xA0/+0xAC/+0xB8
       and each pins its own slot's target). Slot 17 is SetOrigin, NOT SetAngles: the IDA name
       behind the old member here ("CMapMesh::SetAngles", pin HVS_MESH_SET_ANGLES) is wrong. The
       mesh overrides of the same two slots are HVS_MESH_SET_ROTATION (18) and HVS_MESH_SET_SCALES
       (19), the same CMapPoint slots; HVS_MESH_SET_ANGLES (17) is retired but its sigs entry
       stays (never rename a sig). */
    void (*SetOrigin)(CMapNode *self, const float *origin);      /* 17 CMapEntity 0x1808959E0 / CMapMesh 0x1809A23F0 */
    void (*SetAngles)(CMapNode *self, const float *angles);      /* 18 CMapEntity 0x180895A50 / CMapMesh 0x1809A2540 */
    void (*SetScales)(CMapNode *self, const float *scales);      /* 19 CMapEntity 0x180895B40 / CMapMesh 0x1809A2690 */
    void (*GetWorldTransform)(CMapNode *self, float *matrix3x4); /* 20 */
    S2_PAD(21, 56);
    /* 57: CMapNode::AddChild (m2.19, re-triggers.md Q1) - detaches child from its old parent and
       attaches it here (SetParent through the child's slot, then the parent's child list);
       returns 0 on null / cycle / same parent. No transform change: node transforms are
       world-space (GetWorldTransform builds from the node's own origin/angles only), so a
       reparented mesh keeps its world geometry ([V live] 2026-10-16: the tied mesh's origin and
       saved meshData are unchanged). The same slot number in the CMapEntity and CMapMesh
       vtables (and everything between). */
    u8 (*AddChild)(CMapNode *self, CMapNode *child);             /* 57 CMapEntity/CMapMesh 0x18084F670 */
    S2_PAD(58, 139);
    void (*SetPivotPosition)(CMapNode *self, const float *pos);  /* 140 (CMapMesh: keeps world positions) */
} CMapNodeVtbl;

/* component list of CHammerPolyMesh (vertices / faces): count first */
typedef struct { int count; } S2ComponentList;

/* CHammerPolyMesh */
struct CHammerPolyMesh {
    u8 _p0[HOFF_POLYMESH_VERTS];
    S2ComponentList vertices;               /* 0x18 */
    u8 _p1[HOFF_POLYMESH_FACES - HOFF_POLYMESH_VERTS - 4];
    S2ComponentList faces;                  /* 0x88 */
};
S2_FIELD(CHammerPolyMesh, vertices, HOFF_POLYMESH_VERTS);
S2_FIELD(CHammerPolyMesh, faces, HOFF_POLYMESH_FACES);

/* CMapNode (.?AVCMapNode@@) is a CDmElement (-> CMapAtom -> CMapPoint). The generated layout
   carries m_pAttributes (0x48, CDmElement), m_Origin/m_Angles/m_Scales (CMapPoint; scales are not a
   DME attribute, set through slot 19), m_pParent (the world, a group or the brush entity this node
   is tied to; m2.19: read guarded, tieMeshes' already-tied gate and the emptied-entity rule) and
   m_Children (a CUtlVector of DME element handles: count, then the handle array). */
S2_OBJECT(CMapNode, S2_CMapNode, CMapNodeVtbl);
static inline u32 *CMapNode_ChildHandles(const CMapNode *n) { return (u32 *)n->m_Children.m_pMemory; }
/* the IUndoable subobject (RecordNode's argument) */
static inline void *CMapNode_Undoable(CMapNode *n) { return (u8 *)n + S2_BASEOFF_CMapNode_IUndoable; }
S2_SLOT(CMapNodeVtbl, IsA, S2_SLOT_CDmElement_IsA);
S2_SLOT(CMapNodeVtbl, SetOrigin, S2_SLOT_CMapPoint_SetOrigin);
S2_SLOT(CMapNodeVtbl, SetAngles, S2_SLOT_CMapPoint_SetAngles);
S2_SLOT(CMapNodeVtbl, SetScales, S2_SLOT_CMapPoint_SetScales);
S2_SLOT(CMapNodeVtbl, AddChild, S2_SLOT_CMapNode_AddChild);
S2_SLOT(CMapNodeVtbl, GetWorldTransform, S2_SLOT_CMapPoint_GetWorldTransform);
S2_SLOT(CMapNodeVtbl, SetPivotPosition, S2_SLOT_CMapNode_SetPivotPosition);
/* the same three slots on the mesh/entity vtables (re-props.md Q2): the mesh and entity pins
   (HVS_ENTITY_SET_*, HVS_MESH_SET_*) all adopt the same CMapPoint slots, so there is nothing left
   to assert between them; the mesh override of slot 18 is the one the retired HVS_MESH_SET_ANGLES
   name used to point at one cell too early */

/* CMapMesh (.?AVCMapMesh@@): CMapNode + embedded CHammerPolyMesh at +0x498 */
static inline CHammerPolyMesh *CMapMesh_PolyMesh(CMapMesh *m) { return (CHammerPolyMesh *)((u8 *)m + HOFF_MESH_POLYMESH); }
static inline CMapNode *CMapMesh_Node(CMapMesh *m) { return (CMapNode *)m; }

/* ======================================================================================
 * hammer.dll - viewport - (b) none of these types exists upstream
 * ====================================================================================== */
/* CMapView (.?AVCMapView@@) */
typedef struct {
    S2_PAD(0, 5);
    CCamera *(*GetCamera)(CMapView *self);                    /* 6 (returns this+0xDB0) */
} CMapViewVtbl;
/* m_nViewType (>2 = 3D) and m_nRenderMode */
S2_OBJECT(CMapView, S2_CMapView, CMapViewVtbl);
S2_SLOT(CMapViewVtbl, GetCamera, S2_SLOT_IToolRenderableViewHost_GetCamera);

/* CMapViewMgr (.?AVCMapViewMgr@@) */
S2_OBJECT(CMapViewMgr, S2_CMapViewMgr, void);
static inline u32 CMapViewMgr_ViewCount(const CMapViewMgr *m) { return (u32)m->m_Views.m_Size; }

/* camera object: embeds a CFrustum at +0x50 */
static inline CFrustum *CCamera_Frustum(CCamera *c) { return (CFrustum *)((u8 *)c + S2_OFF_ToolCameraUnk_t_m_Frustum); }

/* hammer editor settings (HammerEditorSettings_t, the global HG_EDITOR_SETTINGS): "Show Tools Materials" */
typedef struct {
    u8 _p0[S2_OFF_HammerEditorSettings_t_m_bShowToolsMaterials];
    u8 showToolsMaterials;
} S2HammerEditorSettings;

/* ======================================================================================
 * modeldoc_editor.dll - .vmdl documents - (b) none of these types exists anywhere upstream
 * (hammer-mcp/research/modeldoc-re.md, first pass build 2000930)
 * ====================================================================================== */
/* CModelDocEditorApp (.?AVCModelDocEditorApp@@) */
typedef struct {
    S2_PAD(0, 17);
    /* OpenFile returns the session (0 on failure / re-entrancy guard app+0x1A3); flags 1 is what
       the UI passes. Modals: missing file, outside the content dir, load errors - the Rust tool
       pre-validates and sends only addon-relative paths (models/...). */
    void *(*OpenFile)(CModelDocEditorApp *self, const char *path, u32 flags);    /* 18 */
} CModelDocEditorAppVtbl;

/* CModelDocEditorApp: ToolSystem2_ModelDoc (modeldoc_editor.dll's exported CreateInterface;
 * ret 0 = found), the identity of the static singleton g_CModelDocEditorApp (MDG_APP_OBJ) */
S2_OBJECT(CModelDocEditorApp, S2_CModelDocEditorApp, CModelDocEditorAppVtbl);
static inline CModelDocEditorSession *CModelDocEditorApp_ActiveSession(const CModelDocEditorApp *a) { return (CModelDocEditorSession *)a->m_pActiveSession; }   /* may be 0 (no document open) */
S2_SLOT(CModelDocEditorAppVtbl, OpenFile, S2_SLOT_CBaseEditorApp_OpenFile);

/* CModelDocEditorSession (.?AVCModelDocEditorSession@@); one per open .vmdl file */
typedef struct {
    S2_PAD(0, 6);
    /* ReloadDocument: ResetDocument (new CModelDoc -> doc BELOW is replaced, undo history gone)
       + LoadFromFile(GetPath) + refresh. No prompt, no modal; load errors only logged. */
    void (*ReloadDocument)(CModelDocEditorSession *self);                       /* 7 */
    S2_PAD(8, 10);
    u8 (*IsModified)(CModelDocEditorSession *self);                             /* 11 = doc+0x3BC (0 when no doc) */
    void (*SetModified)(CModelDocEditorSession *self, u8 modified);             /* 12; FileSave calls it with 0 after a save */
    const char *(*GetPath)(CModelDocEditorSession *self);                       /* 13 absolute Windows path ("" when unset) */
    S2_PAD(14, 17);
    /* SaveAndNotify: the FileSave handler passes (path, path, 0, 0) and then slot 12 with 0.
       Writes KV3 text modeldoc41. Never call session slot 21 SaveAndCompile: it opens a modal. */
    u8 (*SaveAndNotify)(CModelDocEditorSession *self, const char *oldPath, const char *newPath,
                        u8 a4, u8 a5);                                          /* 18 */
} CModelDocEditorSessionVtbl;
struct CModelDocEditorSession {
    const CModelDocEditorSessionVtbl *vt;
    u8 _p0[S2_OFF_CBaseEditorSession_m_bIsNew - 8];
    u8 isNewFile;                           /* 0x19 untitled document (FileSave would open Save As) */
    u8 _p1[S2_OFF_CModelDocEditorSession_m_pDoc - S2_OFF_CBaseEditorSession_m_bIsNew - 1];
    void *doc;                              /* 0x118 CModelDoc (vtable MDV_DOC_VT); replaced on every load/reload - never cache */
    u8 _p2[S2_OFF_CModelDocEditorSession_m_pAsset - S2_OFF_CModelDocEditorSession_m_pDoc - 8];
    void *asset;                            /* 0xBF8 AssetSystem001 asset; 0 => Compile is a no-op */
    u8 _p3[S2_OFF_CModelDocEditorSession_m_Analysis - S2_OFF_CModelDocEditorSession_m_pAsset - 8];
    const void *compileStatus;              /* CModelDocAnalysis vptr (an IErrorListener the compile reports to), NOT a string: never read (live 2026-10-12) */
};
S2_SLOT(CModelDocEditorSessionVtbl, ReloadDocument, S2_SLOT_CBaseEditorSession_ReloadDocument);
S2_SLOT(CModelDocEditorSessionVtbl, IsModified, S2_SLOT_CBaseEditorSession_IsModified);
S2_SLOT(CModelDocEditorSessionVtbl, SetModified, S2_SLOT_CBaseEditorSession_SetModified);
S2_SLOT(CModelDocEditorSessionVtbl, GetPath, S2_SLOT_CBaseEditorSession_GetPath);
S2_SLOT(CModelDocEditorSessionVtbl, SaveAndNotify, S2_SLOT_CBaseEditorSession_Save);
S2_FIELD(CModelDocEditorSession, isNewFile, S2_OFF_CBaseEditorSession_m_bIsNew);
S2_FIELD(CModelDocEditorSession, doc, S2_OFF_CModelDocEditorSession_m_pDoc);
S2_FIELD(CModelDocEditorSession, asset, S2_OFF_CModelDocEditorSession_m_pAsset);
S2_FIELD(CModelDocEditorSession, compileStatus, S2_OFF_CModelDocEditorSession_m_Analysis);

/* Non-virtual Compile (no interface slot): bound once from the pinned RVA
 * (g_modBase[PIN_MODELDOC] + MDFN_SESSION_COMPILE) after the module's identity check. `mode` is
 * unused apart from an event payload (callers pass a garbage edx) - pass 0. Synchronous, no modal;
 * saves first when modified; returns nothing useful (never derive success from it). */
typedef void (*ModelDocCompileFn)(CModelDocEditorSession *, int);

/* ======================================================================================
 * pet.dll - .vpcf documents and the operator registry - (b) none of these types exists
 * anywhere upstream (hammer-mcp/research/pet-re.md, first pass build 2000930)
 * ====================================================================================== */
/* CPetApp (.?AVCPetApp@@) */
typedef struct {
    S2_PAD(0, 17);
    /* OpenFile returns the session (0 on failure / re-entrancy guard app+0x1A3); flags 1 is what
       the UI passes. Modals: missing file, outside the content dir, load errors - the Rust tool
       pre-validates and sends only addon-relative paths (particles/...). */
    void *(*OpenFile)(CPetApp *self, const char *path, u32 flags);               /* 18 */
} CPetAppVtbl;

/* CPetApp: ToolSystem2_001 (pet.dll's exported CreateInterface; ret 0 = found), the identity
 * of the static singleton g_CPetApp (PETG_APP_OBJ) */
/* CBaseEditorSharedWindowApp is the 488-byte shared-window base; CPetApp's own members follow it,
   the first being the main window. */
struct CPetApp {
    union { S2_CBaseEditorSharedWindowApp; struct { const CPetAppVtbl *vt; }; };
    void *mainWnd;                          /* CQPetMainWnd (PETV_MAINWND_VT); preview chain for pet.render (m2.13) */
};
_Static_assert(S2_OFFSETOF(struct CPetApp, vt) == 0, "CPetApp::vt is the vptr");
static inline CPetSession *CPetApp_ActiveSession(const CPetApp *a) { return (CPetSession *)a->m_pActiveSession; }   /* may be 0 (no document open) */
S2_FIELD(CPetApp, mainWnd, S2_SIZEOF_CBaseEditorSharedWindowApp);
S2_SLOT(CPetAppVtbl, OpenFile, S2_SLOT_CBaseEditorApp_OpenFile);

/* CPetSession (.?AVCPetSession@@); one per open .vpcf file */
typedef struct {
    S2_PAD(0, 6);
    /* ReloadDocument: LoadFromPath(GetPath) + SetModified(0) + RecompileResource. No prompt, no
       modal, but it does NOT fail gracefully on a file pet cannot parse: live 2026-10-13 three
       inputs recursed until the Wine main thread's stack overflowed and wedged the whole process
       even against SIGTERM, and a wrong root `_class` exited fail-fast (c0000409) - see the "pet
       load robustness" table in protocol-diff-m2.12.md. A document with "no root" is only the
       empty/untitled state. The bridge pre-checks the file (kv3_check.h's vpcf_check) first. */
    void (*ReloadDocument)(CPetSession *self);                                  /* 7 */
    S2_PAD(8, 10);
    u8 (*IsModified)(CPetSession *self);                                        /* 11 = session+0x60 */
    void (*SetModified)(CPetSession *self, u8 modified);                        /* 12; FileSave calls it with 0 after a save */
    const char *(*GetPath)(CPetSession *self);                                  /* 13 absolute Windows path ("" when unset) */
    S2_PAD(14, 17);
    /* Save: SaveToFile(newPath) as KV3 text vpcf66. The FileSave handler passes (path, path, 0, 0)
       and then slot 12 with 0; refuse when the session is untitled (it would open Save As). No
       modal on failure (only a log line). */
    u8 (*Save)(CPetSession *self, const char *oldPath, const char *newPath,
               u8 a4, u8 a5);                                                   /* 18 */
} CPetSessionVtbl;
/* m_bIsNew: untitled document (FileSave would open Save As); m_pDoc: the CPetDoc (vtable PETV_DOC_VT) */
S2_OBJECT(CPetSession, S2_CPetSession, CPetSessionVtbl);
S2_SLOT(CPetSessionVtbl, ReloadDocument, S2_SLOT_CBaseEditorSession_ReloadDocument);
S2_SLOT(CPetSessionVtbl, IsModified, S2_SLOT_CBaseEditorSession_IsModified);
S2_SLOT(CPetSessionVtbl, SetModified, S2_SLOT_CBaseEditorSession_SetModified);
S2_SLOT(CPetSessionVtbl, GetPath, S2_SLOT_CBaseEditorSession_GetPath);
S2_SLOT(CPetSessionVtbl, Save, S2_SLOT_CBaseEditorSession_Save);

/* CUndoHistory (.?AVCUndoHistory@@): pet's whole-document snapshot history (m2.13) */
typedef struct CUndoHistory CUndoHistory;
typedef struct {
    S2_PAD(0, 2);
    u8 (*HasEntries)(CUndoHistory *self);                                        /* 3 */
} CUndoHistoryVtbl;
/* m_pPartner: the paired redo history */
S2_OBJECT(CUndoHistory, S2_CUndoHistory, CUndoHistoryVtbl);
S2_SLOT(CUndoHistoryVtbl, HasEntries, S2_SLOT_CUndoHistory_HasEntries);

/* CPetDoc (.?AVCPetDoc@@) */
/* m_pRoot (CPetRootNode, kept void *): 0 = the document has no root (an empty or untitled document).
   NOT what a failed load does: pet recurses until the main thread's stack overflows and the process
   wedges (live 2026-10-13), hence the bridge's pre-load gate (kv3_check.h). m_pUndoHistory: the undo
   history (redo = its m_pPartner). */
S2_OBJECT(CPetDoc, S2_CPetDoc, void);

/* ======================================================================================
 * Non-virtual, non-exported functions: typed tables bound from pinned RVAs
 * (b) these have no interface and no export and no upstream header declares them, so the C
 * signatures below are hand-reversed from the DLLs. The RVAs are HFN_ / SFN_ / EFN_ pins from
 * bridge_pins.h; pins.py re-finds each one in the module before it is used (byte-pinned
 * prologues, fail closed).
 * ====================================================================================== */
typedef struct {
    /* undo (CMapDocHistory) */
    CMapDocHistory *(*GetUndoHistory)(CMapDoc *doc);
    CMapDocHistory *(*GetRedoHistory)(CMapDoc *doc);   /* the root's pair partner [root+0x248] (m2.17) */
    CMapDocHistory *(*GetActiveHistory)(void);
    void (*AddOperation)(CMapDocHistory *hist, CSelection *sel, const char *name, long long flags);
    void (*RecordNodeTree)(CMapDocHistory *hist, void *undoable, int flags);
    void (*RecordNode)(CMapDocHistory *hist, void *undoable, int flags);
    /* m2.19 (re-triggers.md Q1): the reparent recording + the untie primitive. RecordNodesAndParents
       records each node in the (caller-owned stack) CUtlVector<CMapNode*> and, when it has a
       parent, RecordNodeTree(parent, 0) non-recursive - it must run BEFORE the AddChild calls
       (Hammer's own order) or the undo would restore the wrong child lists. MoveToWorld opens its
       OWN undo group ("Untie from entity"), so callers must not undo_open around it; it reads only
       the vector's count and data pointer and ignores non-entity entries. */
    void (*RecordNodesAndParents)(CMapDocHistory *hist, const void *nodeVec, int recurse);
    void (*DocMoveToWorld)(CMapDoc *doc, const void *nodeVec);
    /* document */
    void (*ActivateDoc)(CMapDoc *doc, int activate);
    int (*DocOrDependencyChanged)(CMapDoc *doc, int unused);
    int (*ChangedSinceSave)(void *docDataManager, CMapDoc *doc);
    void (*DocSetModified)(CMapDoc *doc, void *dataKey);   /* CMapDoc::SetModified; callers pass (doc, doc) (m2.15) */
    void *(*FindTypeByName)(void *factoryRegistry, const char *className);
    CMapNode *(*CreateNodeByName)(CMapDoc *doc, const char *className, float *origin, float *angles);
    void (*InsertNode)(CMapDoc *doc, CMapNode *node, CMapNode *parent);
    void (*DeleteNode)(CMapDoc *doc, CMapNode *node);
    u8 (*NodeIsLocked)(CMapNode *node, int unused);
    CDmElement *(*GetEntityProperties)(CMapNode *entity);
    /* DME attributes */
    CDmAttribute *(*FindAttribute)(CDmElement *elem, const S2_CUtlSymbolLarge *nameSymbol);
    CDmAttribute *(*CreateAttribute)(CDmElement *elem, const S2_CUtlSymbolLarge *nameSymbol, u32 type);
    void (*SetValueString)(CDmAttribute *attr, const S2_CUtlSymbolLarge *value);   /* stores the symbol, no copy */
    void (*SetValueInt)(CDmAttribute *attr, const int *value);
    void (*SetValueFloat)(CDmAttribute *attr, const float *value);
    /* CMapGameDataNode::SetKeyValue (m2.18, re-props.md Q1): Hammer's own keyvalue setter. Writes
       the entity_properties String attribute of `key` - creating it as S2_AT_STRING when missing,
       matching an existing key case-insensitively - and, when the value changed, calls the node's
       OnKeyValueChanged handler (CMapEntity's rebuilds the helpers/CModelHelper, which is what
       makes a prop model load and draw). Requires GetEntityProperties(node) to have returned
       non-null: the element is dereferenced unguarded. `value` is a plain C string it interns. */
    void (*SetKeyValue)(CMapNode *node, const char *key, const char *value);
    /* CMapEntityIONode I/O (m2.18, re-props.md Q3) */
    void *(*AddConnection)(CMapNode *node, u32 *outId, const S2ConnectionSrc *src);   /* appends 80 B; *outId = index << 16 */
    u8 (*RemoveConnection)(CMapNode *node, u32 id);                                    /* id = index << 16; entries shift */
    void *(*Kv3Init)(void *kv, u8 type, u8 subtype);                                   /* the src paramMap (type 1, sub 8) */
    void (*Kv3Free)(void *kv);                                                         /* pairs Kv3Init */
    /* meshes */
    CMapMesh *(*CreateMeshElement)(u32 fileId, u64 name, const char *srcFile, int srcLine);
    void (*MeshInitFromPolygons)(CMapMesh *mesh, void *polygonGroup, const char *material, int uvMode, S2HandleVec *outFaces);
    void (*AssignMaterialToFaces)(CHammerPolyMesh *hm, const S2MeshHandle *faces, int count, const char *material);
    u8 (*CollectHandles)(S2ComponentList *list, S2HandleVec *out);
    void (*MeshDeleteFaces)(CMapMesh *mesh, S2HandleVec *faces);
    /* viewport / offscreen render */
    CMapView *(*GetPreferred3DView)(CMapViewMgr *mgr);
    void (*SessionUpdateViews)(CHammerEditorSession *session);
    u64 (*RenderViewToMemory)(CMapViewMgr *mgr, CFrustum *frustum, int w, int h, int format, void *pixels, CMapView *templateView);
    void (*FrustumInit)(CFrustum *f);
    void (*FrustumUpdate)(CFrustum *f);
    void (*CameraGetPosition)(CCamera *cam, float *out);
    void (*CameraGetAngles)(CCamera *cam, float *out);
    void (*ToggleToolsMaterials)(CHammerApp *app);
} HammerFns;

typedef struct {
    void (*FrustumInit)(CFrustum *f);
    void (*FrustumUpdate)(CFrustum *f);
} SceneFns;

typedef struct {
    void (*AttrsCtor)(S2RenderAttrs *attrs);
    void (*AttrsDtor)(S2RenderAttrs *attrs);
    void (*AttrsSetHandle)(void *handleTable, u32 token, long long handle);
} EngineFns;


#endif /* S2SDK_H */
