//===== s2sdk-sys binding generator: clang probe translation unit ==============//
//
// This file is compiled by s2sdk-sys/gen/gen.py (never by the Rust build) to
// make clang print what the bindings need. It includes exactly the headers that
// gen/allowlist.toml lists, with the upstream Linux include dirs / defines that
// gen.py reads out of the SDK's CMakeLists.txt, and it exists to *use* the
// allowlisted types: clang only prints a record layout or a vtable for a type
// it was asked to lay out or emit.
//
// Three clang passes read this file:
//
//   1. -Xclang -ast-dump=json
//        record/enum declarations, field types and declaration order, and the
//        declarations of each class's own virtuals (which methods exist, in
//        what order, their signatures, dtors, pure/override).
//   2. -Xclang -fdump-record-layouts
//        the sizes / alignments / field offsets / base offsets / vtable pointer
//        markers of every laid-out record (the JSON AST has no offsets).
//        A record is only laid out if something needs its size, which is what
//        S2SDK_FORCE_LAYOUT below is for.
//   3. -Xclang -fdump-vtable-layouts (-S, codegen)
//        the Itanium slot of every virtual. clang emits a vtable only when
//        something in the TU uses it, and only at codegen time, so
//        S2SDK_FORCE_VTABLE below takes the address of one virtual *declared by
//        the class itself* from a function that is compiled. Taking the address
//        of an inherited method marks the base's vtable instead; a class with no
//        virtual of its own (CSchemaSystemTypeScope) has no vtable to dump.
//
// gen.py checks this file against allowlist.toml: every record/opaque/vtable
// name has to appear in exactly one S2SDK_FORCE_LAYOUT row, every forced vtable
// method has to be a virtual the class declares itself (one row per vtable
// class, and a class that declares virtuals needs a row), every `specs` entry
// needs its S2SDK_FORCE_SPEC row, and the includes here have to be the
// allowlist's headers. A typo therefore fails the run instead of silently
// shifting a slot or an offset.
//
// The includes also decide which macros the `[consts]` expressions can name;
// nothing below the includes is a layout for them: gen.py expands those
// expressions in its own generated TU and reads the values back.

// tier0 ----------------------------------------------------------------------
#include "tier0/platform.h"
#include "appframework/iappsystem.h"
#include "tier0/utlstring.h"
#include "tier0/utlsymbollarge.h"
#include "tier0/buffer_string.h"
#include "mathlib/vector.h"
#include "color.h"
#include "mathlib/vector2d.h"
#include "mathlib/vector4d.h"
#include "tier0/utlstringtoken.h"
#include "tier0/keyvalues3.h"
#include "tier0/interface.h"
#include "tier0/utlvector.h"
#include "tier0/itestscriptmgr.h"
#include "tier0/basetypes.h"

// schema ---------------------------------------------------------------------
#include "schemasystem/minschema.h"
#include "schemasystem/baseschema.h"
#include "schemalib/schematype.h"
#include "schemalib/schemaclassinfo_data.h"
#include "schemalib/schemametadata_data.h"
#include "schemasystem/ischemasystem.h"
#include "schemasystem/schemasystem.h"
#include "schemalib/schemaclassinfo.h"

// entity ---------------------------------------------------------------------
#include "tier0/basetypes.h"
#include "entityhandle.h"
#include "entity2/entityidentity.h"
#include "entity2/entityinstance.h"

// cvar -----------------------------------------------------------------------
#include "tier0/convar.h"
#include "tier0/icvar.h"

// events ---------------------------------------------------------------------
#include "igameevents.h"

// engine ---------------------------------------------------------------------
#include "engine2/isource2serverconfig.h"
#include "playerslot.h"
#include "rendersystem/rendersystemtypes.h"
#include "engine2/iengineservicemgr.h"
#include "engine2/irenderservice.h"
#include "engine2/iinputservice.h"
#include "engine2/isource2engine.h"
#include "engine2/ivengineclient2.h"
#include "engine2/isource2server.h"

// toolframework2 -------------------------------------------------------------
#include "toolframework2/itoolframework2.h"

// rendersystem ---------------------------------------------------------------
#include "bitmap/imageformat_declarations.h"
#include "rendersystem/rendersystemtypes.h"
#include "rendersystem/irenderdevice.h"

// scenesystem ----------------------------------------------------------------
#include "mathlib/camera.h"
#include "scenesystem/iscenesystem.h"
#include "scenesystem/isceneutils.h"

// soundsystem ----------------------------------------------------------------
#include "soundsystem/isoundopsystem.h"

// particles ------------------------------------------------------------------
#include "tier0/utlsoacontainer.h"
#include "particles/iparticlesystemmgr.h"

// client ---------------------------------------------------------------------
#include "clientinput.h"
#include "cstrike15/cs_clientinput.h"

// datamodel ------------------------------------------------------------------
#include "datamodel/dmelementhandle.h"
#include "datamodel/dmattributetypes.h"
#include "datamodel/idatamodel.h"
#include "datamodel/dmelement.h"
#include "datamodel/dmattribute.h"

// toolutils2 -----------------------------------------------------------------
#include "toolutils2/ibindtarget.h"
#include "toolutils2/itoolsresourcelistener.h"
#include "toolutils2/undohistory.h"
#include "toolutils2/toolattr.h"
#include "toolutils2/baseeditorsession.h"
#include "toolutils2/baseeditorapp.h"
#include "toolutils2/toolnodetypename.h"
#include "toolutils2/itoolrenderableviewhost.h"
#include "toolutils2/ibindablecamerascene.h"

// pet ------------------------------------------------------------------------
#include "petdoc_lib/petdoc.h"
#include "pet/petapp.h"

// modeldoc_editor ------------------------------------------------------------
#include "tier0/errorlistener.h"
#include "modeldoc_editor/modeldoc_editor_app.h"

// mapdoclib ------------------------------------------------------------------
#include "mapdoclib/mapnode.h"
#include "mapdoclib/mapentity.h"
#include "mapdoclib/mapmesh.h"
#include "mapdoclib/mapdoc.h"

// hammer ---------------------------------------------------------------------
#include "hammer/hammereditorsession.h"
#include "hammer/mapview.h"
#include "hammer/mapviewmgr.h"

// metamod --------------------------------------------------------------------
// The second upstream (Metamod:Source + its KHook submodule), included last so
// every s2sdk header it needs is already complete: ISmmAPI.h constructs a
// `CPlayerSlot` under META_IS_SOURCE2 (`playerslot.h`, ISmmAPI.h:68-70), and
// META_NO_HL2SDK keeps its hl2sdk includes out of the TU entirely (ISmmAPI.h:40-61).
#include "playerslot.h"
#include "core/ISmmAPI.h"
#include "core/IPluginManager.h"
#include "core/ISmmPlugin.h"
#include "khook.hpp"

// consts ---------------------------------------------------------------------
// The headers that declare the `[consts]` macros/constexprs. Nothing here is a
// layout: gen.py expands the allowlist's expressions against this TU and reads
// the values back, so the compiler - not the allowlist - owns them.
#include "const.h"
#include "tier0/generichash.h"
#include "tier0/utlvectormemory.h"
#include "appframework/iappsystem.h"
#include "interfaces/interfaces.h"
#include "bitmap/imageformat_declarations.h"
// metamod's plugin-API version/name macros; khook.hpp (above) has HookID_t's
// INVALID_HOOK. The two upstreams' headers all reach this one TU, so a const
// expression may name either.
#include "core/ISmmPluginExt.h"
// the FCVAR_* ConVar flag bits (also reached through the cvar group above).
#include "tier0/convar.h"

// --- record layouts ---------------------------------------------------------
// sizeof forces clang to lay the record out, which is what makes pass 2 print
// it. Abstract classes and classes with no data member are fine here.
#define S2SDK_FORCE_LAYOUT( name ) \
	static const unsigned long s2sdk_layout_ ## name = sizeof( name )

// tier0
S2SDK_FORCE_LAYOUT( CUtlString );
S2SDK_FORCE_LAYOUT( CUtlSymbolLarge );
S2SDK_FORCE_LAYOUT( Vector );
S2SDK_FORCE_LAYOUT( QAngle );
S2SDK_FORCE_LAYOUT( Color );
S2SDK_FORCE_LAYOUT( Vector2D );
S2SDK_FORCE_LAYOUT( Vector4D );
S2SDK_FORCE_LAYOUT( CUtlStringToken );
S2SDK_FORCE_LAYOUT( CKV3MemberName );
S2SDK_FORCE_LAYOUT( CSplitScreenSlot );
S2SDK_FORCE_LAYOUT( CBufferString );
S2SDK_FORCE_LAYOUT( IAppSystem );
S2SDK_FORCE_LAYOUT( IBaseInterface );
S2SDK_FORCE_LAYOUT( ITestScriptMgr );

// toolframework2
S2SDK_FORCE_LAYOUT( ToolActivationRequest_t );
S2SDK_FORCE_LAYOUT( ToolOpenAssetEntry_t );
S2SDK_FORCE_LAYOUT( MultiAxisControllerInput_t );
S2SDK_FORCE_LAYOUT( IToolFramework2 );
S2SDK_FORCE_LAYOUT( IToolSystem2 );
S2SDK_FORCE_LAYOUT( IToolFrameListener );

// scenesystem
S2SDK_FORCE_LAYOUT( Camera_t );
S2SDK_FORCE_LAYOUT( CFrustum );
S2SDK_FORCE_LAYOUT( CViewId );
S2SDK_FORCE_LAYOUT( CRenderAttributes );
S2SDK_FORCE_LAYOUT( HMaterialWeak );
S2SDK_FORCE_LAYOUT( SceneViewUnk16_t );
S2SDK_FORCE_LAYOUT( SceneViewUnk4_t );
S2SDK_FORCE_LAYOUT( ISceneSystem );
S2SDK_FORCE_LAYOUT( ISceneView );
S2SDK_FORCE_LAYOUT( IRenderingPipeline );
S2SDK_FORCE_LAYOUT( ISceneUtils );

// soundsystem
S2SDK_FORCE_LAYOUT( SoundEventGuid_t );
S2SDK_FORCE_LAYOUT( SoundGuid_t );
S2SDK_FORCE_LAYOUT( SosStartSoundEventParams_t );
S2SDK_FORCE_LAYOUT( ISoundEventListener );
S2SDK_FORCE_LAYOUT( ISoundEventManager );
S2SDK_FORCE_LAYOUT( ISoundOpSystem );

// particles
S2SDK_FORCE_LAYOUT( HParticleSystemDefinition );
S2SDK_FORCE_LAYOUT( HParticleSnapshot );
S2SDK_FORCE_LAYOUT( ParticleSnapshotAttributeMap_t );
S2SDK_FORCE_LAYOUT( IParticleSystemMgr );
S2SDK_FORCE_LAYOUT( IParticleFunctionDefinition );

// rendersystem
S2SDK_FORCE_LAYOUT( HRenderTexture );
S2SDK_FORCE_LAYOUT( RenderDeviceUnkVector4_t );
S2SDK_FORCE_LAYOUT( IRenderDevice );

// schema
S2SDK_FORCE_LAYOUT( CSchemaType );
S2SDK_FORCE_LAYOUT( SchemaClassInfoData_t );
S2SDK_FORCE_LAYOUT( SchemaClassFieldData_t );
S2SDK_FORCE_LAYOUT( SchemaStaticFieldData_t );
S2SDK_FORCE_LAYOUT( SchemaMetadataEntryData_t );
S2SDK_FORCE_LAYOUT( SchemaBaseClassInfoData_t );
S2SDK_FORCE_LAYOUT( CSchemaClassInfo );
S2SDK_FORCE_LAYOUT( ISchemaSystem );
S2SDK_FORCE_LAYOUT( ISchemaSystemTypeScope );
S2SDK_FORCE_LAYOUT( CSchemaSystemTypeScope );

// entity
S2SDK_FORCE_LAYOUT( CEntityHandle );
S2SDK_FORCE_LAYOUT( CEntityIndex );
S2SDK_FORCE_LAYOUT( CEntityIdentity );
S2SDK_FORCE_LAYOUT( CEntityInstance );

// cvar
S2SDK_FORCE_LAYOUT( ConVarData );
S2SDK_FORCE_LAYOUT( CVValue_t );
S2SDK_FORCE_LAYOUT( ConVarRef );
S2SDK_FORCE_LAYOUT( ConVarRefAbstract );
S2SDK_FORCE_LAYOUT( ICvar );

// events
S2SDK_FORCE_LAYOUT( IGameEvent );
S2SDK_FORCE_LAYOUT( IGameEventListener2 );
S2SDK_FORCE_LAYOUT( IGameEventManager2 );

// engine
S2SDK_FORCE_LAYOUT( CPlayerSlot );
S2SDK_FORCE_LAYOUT( RenderViewport_t );
S2SDK_FORCE_LAYOUT( ISource2ServerConfig );
S2SDK_FORCE_LAYOUT( EngineClientUnk40_t );
S2SDK_FORCE_LAYOUT( IEngineService );
S2SDK_FORCE_LAYOUT( IRenderService );
S2SDK_FORCE_LAYOUT( IInputService );
S2SDK_FORCE_LAYOUT( IInputHandler );
S2SDK_FORCE_LAYOUT( IKeybindChangeListener );
S2SDK_FORCE_LAYOUT( ISource2Engine );
S2SDK_FORCE_LAYOUT( IVEngineClient2 );
S2SDK_FORCE_LAYOUT( ISource2Server );

// client
S2SDK_FORCE_LAYOUT( CInputValueBase );
S2SDK_FORCE_LAYOUT( ClientInputButton_t );
S2SDK_FORCE_LAYOUT( CClientInput );
S2SDK_FORCE_LAYOUT( CCSGOInput );

// datamodel
S2SDK_FORCE_LAYOUT( IDataModel );
S2SDK_FORCE_LAYOUT( DmAttributeList_t );
S2SDK_FORCE_LAYOUT( DmElementReference_t );
S2SDK_FORCE_LAYOUT( CDmElement );
S2SDK_FORCE_LAYOUT( CDmAttribute );

// toolutils2
S2SDK_FORCE_LAYOUT( IBindTarget );
S2SDK_FORCE_LAYOUT( IToolsResourceListener );
S2SDK_FORCE_LAYOUT( IUndoable );
S2SDK_FORCE_LAYOUT( IUndoListener );
S2SDK_FORCE_LAYOUT( CBaseUndoable );
S2SDK_FORCE_LAYOUT( UndoGroup_t );
S2SDK_FORCE_LAYOUT( CUndoHistory );
S2SDK_FORCE_LAYOUT( ToolAttrUnk_t );
S2SDK_FORCE_LAYOUT( IToolAttr_Table );
S2SDK_FORCE_LAYOUT( IToolAttrNotifyRoot );
S2SDK_FORCE_LAYOUT( CBaseKV3DataWithFGDClass );
S2SDK_FORCE_LAYOUT( CBaseEditorSession );
S2SDK_FORCE_LAYOUT( CSimpleBaseEditorSession );
S2SDK_FORCE_LAYOUT( CSimpleSharedWindowEditorSession );
S2SDK_FORCE_LAYOUT( CBaseToolSystem2 );
S2SDK_FORCE_LAYOUT( CBaseEditorApp );
S2SDK_FORCE_LAYOUT( CBaseEditorSharedWindowApp );
S2SDK_FORCE_LAYOUT( CBaseEditorSeparateWindowApp );
S2SDK_FORCE_LAYOUT( CToolNodeTypeName );
S2SDK_FORCE_LAYOUT( ToolCameraUnk_t );
S2SDK_FORCE_LAYOUT( ToolRenderableViewHostUnk8_t );
S2SDK_FORCE_LAYOUT( IToolRenderableViewHost );
S2SDK_FORCE_LAYOUT( IBindableCameraScene );

// pet
S2SDK_FORCE_LAYOUT( CBasePetNode );
S2SDK_FORCE_LAYOUT( PetDocSelection_t );
S2SDK_FORCE_LAYOUT( CPetDoc );
S2SDK_FORCE_LAYOUT( CPetSession );
S2SDK_FORCE_LAYOUT( CPetApp );

// modeldoc_editor
S2SDK_FORCE_LAYOUT( IAssetFingerprintListener );
S2SDK_FORCE_LAYOUT( IErrorListener );
S2SDK_FORCE_LAYOUT( CModelDocAnalysis );
S2SDK_FORCE_LAYOUT( CModelDocEditorSession );
S2SDK_FORCE_LAYOUT( CModelDocEditorApp );

// mapdoclib
S2SDK_FORCE_LAYOUT( MapNodeUnk8_t );
S2SDK_FORCE_LAYOUT( MapNodeUnk4_t );
S2SDK_FORCE_LAYOUT( IToolRenderCallbacks );
S2SDK_FORCE_LAYOUT( IHitTestable );
S2SDK_FORCE_LAYOUT( ITransformable );
S2SDK_FORCE_LAYOUT( IMapNodeToolAttrRoot );
S2SDK_FORCE_LAYOUT( IRaytraceEnvironmentOwner );
S2SDK_FORCE_LAYOUT( CMapAtom );
S2SDK_FORCE_LAYOUT( CMapPoint );
S2SDK_FORCE_LAYOUT( CMapNode );
S2SDK_FORCE_LAYOUT( MapEntityConnection_t );
S2SDK_FORCE_LAYOUT( FgdClass_t );
S2SDK_FORCE_LAYOUT( IVDataLookupListener );
S2SDK_FORCE_LAYOUT( CMapEntityIONode );
S2SDK_FORCE_LAYOUT( CMapGameDataNode );
S2SDK_FORCE_LAYOUT( CMapEntity );
S2SDK_FORCE_LAYOUT( IHammerMeshChangeListener );
S2SDK_FORCE_LAYOUT( CMapMeshBase );
S2SDK_FORCE_LAYOUT( CMapMesh );
S2SDK_FORCE_LAYOUT( IMapFileReference );
S2SDK_FORCE_LAYOUT( IUndoDoc );
S2SDK_FORCE_LAYOUT( CMapDocHistory );
S2SDK_FORCE_LAYOUT( CMapRootElement );
S2SDK_FORCE_LAYOUT( CEditScopeStack );
S2SDK_FORCE_LAYOUT( CMapDoc );

// hammer
S2SDK_FORCE_LAYOUT( HammerEditorSettings_t );
S2SDK_FORCE_LAYOUT( IBindableCommandListener );
S2SDK_FORCE_LAYOUT( IAssetFileListener );
S2SDK_FORCE_LAYOUT( IMapDocSession );
S2SDK_FORCE_LAYOUT( CMapDocSession );
S2SDK_FORCE_LAYOUT( CHammerEditorSession );
S2SDK_FORCE_LAYOUT( CMapView );
S2SDK_FORCE_LAYOUT( ITagVisualizationListener );
S2SDK_FORCE_LAYOUT( IShaderReloadListener );
S2SDK_FORCE_LAYOUT( IBoundingBoxChangeListener );
S2SDK_FORCE_LAYOUT( CMapViewMgr );

// consts
// (no records: the `[consts]` entries are expressions, evaluated in the
// generated probe TU, not types to lay out)

// metamod
// The second upstream: the plugin-facing interfaces. ISmmPlugin.h ends with
// `using namespace SourceMM` of its own (METAMOD_NO_AUTO_NAMESPACE is not
// defined), so its names are already reachable; KHook has no such switch, and
// every S2SDK_FORCE_* row needs a plain identifier.
using namespace KHook;

S2SDK_FORCE_LAYOUT( ISmmPlugin );
S2SDK_FORCE_LAYOUT( IMetamodListener );
S2SDK_FORCE_LAYOUT( ISmmAPI );
S2SDK_FORCE_LAYOUT( ISmmPluginManager );
S2SDK_FORCE_LAYOUT( IKHook );

// --- vtables ----------------------------------------------------------------
// One row per class that declares virtuals of its own. The method has to be
// declared by that class (an inherited one only marks the base's vtable) and
// must not be overloaded, so `&class::method` is unambiguous.
#define S2SDK_FORCE_VTABLE( name, method ) \
	void s2sdk_vtable_ ## name () { (void)& name :: method; }

// C++ forbids taking the address of a destructor, so the class whose only own
// virtual is its destructor forces the vtable with a virtual destructor call
// through a pointer instead - which is exactly the vtable load the compiler
// has to lay out.
#define S2SDK_FORCE_VTABLE_DTOR( name ) \
	void s2sdk_vtable_ ## name ( name * s2sdk_this ) { s2sdk_this->~ name (); }

S2SDK_FORCE_VTABLE( IAppSystem, GetTier );
S2SDK_FORCE_VTABLE_DTOR( IBaseInterface );
S2SDK_FORCE_VTABLE( ITestScriptMgr, InitFromCommandLine );

S2SDK_FORCE_VTABLE( IToolFramework2, StartStallMonitor );
S2SDK_FORCE_VTABLE( IToolSystem2, InitToolSystem );
S2SDK_FORCE_VTABLE( IToolFrameListener, RunFrame );

S2SDK_FORCE_VTABLE( ISceneSystem, CreateSceneObject );
S2SDK_FORCE_VTABLE( ISceneView, GetName );
S2SDK_FORCE_VTABLE( IRenderingPipeline, AddLayersToView );
S2SDK_FORCE_VTABLE( ISceneUtils, AddSimpleLayerToView );

S2SDK_FORCE_VTABLE( ISoundEventListener, OnStartSoundEventMessages );
S2SDK_FORCE_VTABLE( ISoundEventManager, GetSoundEventHash );
S2SDK_FORCE_VTABLE( ISoundOpSystem, StartStack );

S2SDK_FORCE_VTABLE( IParticleSystemMgr, SetFallbackParameters );
S2SDK_FORCE_VTABLE( IParticleFunctionDefinition, GetName );

// CCSGOInput declares no virtual of its own; its table is CClientInput's with 23 overrides, so only
// CClientInput names a method. The destructor is the first virtual and CCSGOInput's is implicit.
S2SDK_FORCE_VTABLE( CClientInput, Init );

S2SDK_FORCE_VTABLE( IDataModel, AddElementFactory );
S2SDK_FORCE_VTABLE( CDmElement, OnAttributeChanged );

S2SDK_FORCE_VTABLE( IBindTarget, unk101 );
S2SDK_FORCE_VTABLE( IToolsResourceListener, unk001 );
S2SDK_FORCE_VTABLE( IUndoable, unk001 );
S2SDK_FORCE_VTABLE( IUndoListener, unk001 );
S2SDK_FORCE_VTABLE_DTOR( CBaseUndoable );
S2SDK_FORCE_VTABLE( CUndoHistory, unk101 );
S2SDK_FORCE_VTABLE( IToolAttr_Table, unk001 );
S2SDK_FORCE_VTABLE( IToolAttrNotifyRoot, unk001 );
S2SDK_FORCE_VTABLE( CBaseKV3DataWithFGDClass, unk111 );
S2SDK_FORCE_VTABLE( CBaseEditorSession, unk101 );
S2SDK_FORCE_VTABLE( CBaseEditorApp, unk071 );
S2SDK_FORCE_VTABLE( CBaseEditorSharedWindowApp, unk081 );
S2SDK_FORCE_VTABLE( CBasePetNode, unk021 );
S2SDK_FORCE_VTABLE( CPetDoc, unk021 );
S2SDK_FORCE_VTABLE( CPetSession, unk031 );
S2SDK_FORCE_VTABLE( IAssetFingerprintListener, unk001 );
S2SDK_FORCE_VTABLE( IErrorListener, ReportMessage );
S2SDK_FORCE_VTABLE( IToolRenderableViewHost, unk001 );
S2SDK_FORCE_VTABLE( IBindableCameraScene, unk001 );

// mapdoclib: CMapRootElement, CMapDocHistory, CEditScopeStack and CMapDoc declare no virtual of their own
// (overrides only), so their tables are their bases'.
S2SDK_FORCE_VTABLE( IToolRenderCallbacks, unk001 );
S2SDK_FORCE_VTABLE( IHitTestable, unk001 );
S2SDK_FORCE_VTABLE( ITransformable, unk001 );
S2SDK_FORCE_VTABLE( IMapNodeToolAttrRoot, unk011 );
S2SDK_FORCE_VTABLE( IRaytraceEnvironmentOwner, unk001 );
S2SDK_FORCE_VTABLE( CMapAtom, unk011 );
S2SDK_FORCE_VTABLE( CMapPoint, SetOrigin );
S2SDK_FORCE_VTABLE( CMapNode, unk031 );
S2SDK_FORCE_VTABLE( IVDataLookupListener, unk001 );
S2SDK_FORCE_VTABLE( CMapEntityIONode, unk041 );
S2SDK_FORCE_VTABLE( CMapGameDataNode, unk051 );
S2SDK_FORCE_VTABLE( CMapEntity, unk061 );
S2SDK_FORCE_VTABLE( IHammerMeshChangeListener, unk001 );
S2SDK_FORCE_VTABLE( CMapMeshBase, unk041 );
S2SDK_FORCE_VTABLE( CMapMesh, unk051 );
S2SDK_FORCE_VTABLE( IMapFileReference, unk101 );
S2SDK_FORCE_VTABLE( IUndoDoc, unk001 );

// hammer: CMapView and CMapViewMgr declare no virtual of their own.
S2SDK_FORCE_VTABLE( IBindableCommandListener, unk001 );
S2SDK_FORCE_VTABLE( IAssetFileListener, unk001 );
S2SDK_FORCE_VTABLE( IMapDocSession, unk001 );
S2SDK_FORCE_VTABLE_DTOR( CMapDocSession );
S2SDK_FORCE_VTABLE( CHammerEditorSession, GetMapFilePath );
S2SDK_FORCE_VTABLE( ITagVisualizationListener, unk001 );
S2SDK_FORCE_VTABLE( IShaderReloadListener, unk001 );
S2SDK_FORCE_VTABLE( IBoundingBoxChangeListener, unk001 );

S2SDK_FORCE_VTABLE( ISchemaSystem, FindTypeScopeForModule );
S2SDK_FORCE_VTABLE( ISchemaSystemTypeScope, GetScopeName );
S2SDK_FORCE_VTABLE( CSchemaType, IsValid );

S2SDK_FORCE_VTABLE( CEntityInstance, Connect );

S2SDK_FORCE_VTABLE( ICvar, FindFirstConVar );

S2SDK_FORCE_VTABLE( IGameEvent, GetName );
S2SDK_FORCE_VTABLE( IGameEventListener2, FireGameEvent );
S2SDK_FORCE_VTABLE( IGameEventManager2, LoadEventsFromFile );

// ISource2ServerConfig's own virtuals: the bridge's SRVCFG_VT_SLOTS 29 is
// IAppSystem's 11 plus these 18.
S2SDK_FORCE_VTABLE( ISource2ServerConfig, GetGameDescription );

// The engine services and the client's engine interface. IEngineService declares a
// virtual destructor last, so the first own virtual is the one to name.
S2SDK_FORCE_VTABLE( IEngineService, GetServiceDependencies );
S2SDK_FORCE_VTABLE( IRenderService, InstallRenderHandler );
S2SDK_FORCE_VTABLE( IInputService, InstallInputHandler );
S2SDK_FORCE_VTABLE( IInputHandler, HandleInputEvent );
S2SDK_FORCE_VTABLE( IKeybindChangeListener, unk001 );
S2SDK_FORCE_VTABLE( ISource2Engine, IsPaused );
S2SDK_FORCE_VTABLE( IVEngineClient2, GetSteamUniverse );
// The server game DLL interface (Source2Server001): the bridge hooks GameFrame.
S2SDK_FORCE_VTABLE( ISource2Server, GameFrame );

S2SDK_FORCE_VTABLE( IRenderDevice, GetDeviceAPI );

// --- class-template specializations -----------------------------------------
// `SchemaMetaInfoHandle_t<T>` is the one-pointer wrapper several interface
// methods take or return by value, and gen.py maps it to `*mut T` - but only
// after the AST's `definitionData.canPassInRegisters` says the ABI passes it in
// registers and the record-layout dump shows the single `m_pObj` pointer. A
// specialization is only ever *declared* until something needs its definition,
// and an undeclared-as-defined specialization carries no `definitionData` at
// all, so one row per specialization the IR spells out forces both: `sizeof`
// makes clang lay it out, the member access makes it instantiate.
// (S2SDK_FORCE_LAYOUT cannot be used here: its token pasting needs a plain
// identifier, which a template-id is not.)
#define S2SDK_FORCE_SPEC( tag, type, member ) \
	static const unsigned long s2sdk_spec_size_ ## tag = sizeof( type ); \
	void * s2sdk_spec_ ## tag ( type * s2sdk_obj ) { return s2sdk_obj->member; }

// The same for a specialization whose accessor is not a data member: the row
// still has to name something that forces the definition, and a public method
// does (CUtlVector keeps `m_Size` protected, `Base()` is public).
#define S2SDK_FORCE_SPEC_FIELD( tag, type, member ) \
	static const unsigned long s2sdk_spec_size_ ## tag = sizeof( type ); \
	unsigned long s2sdk_spec_ ## tag ( type * s2sdk_obj ) { return (unsigned long)s2sdk_obj->member; }

S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaClassInfo, SchemaMetaInfoHandle_t<CSchemaClassInfo>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaEnumInfo, SchemaMetaInfoHandle_t<CSchemaEnumInfo>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Ptr, SchemaMetaInfoHandle_t<CSchemaType_Ptr>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Atomic, SchemaMetaInfoHandle_t<CSchemaType_Atomic>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Atomic_T, SchemaMetaInfoHandle_t<CSchemaType_Atomic_T>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Atomic_CollectionOfT, SchemaMetaInfoHandle_t<CSchemaType_Atomic_CollectionOfT>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Atomic_TT, SchemaMetaInfoHandle_t<CSchemaType_Atomic_TT>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Atomic_I, SchemaMetaInfoHandle_t<CSchemaType_Atomic_I>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_DeclaredClass, SchemaMetaInfoHandle_t<CSchemaType_DeclaredClass>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_DeclaredEnum, SchemaMetaInfoHandle_t<CSchemaType_DeclaredEnum>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_FixedArray, SchemaMetaInfoHandle_t<CSchemaType_FixedArray>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Bitfield, SchemaMetaInfoHandle_t<CSchemaType_Bitfield>, m_pObj );
S2SDK_FORCE_SPEC( SchemaMetaInfoHandle_CSchemaType_Builtin, SchemaMetaInfoHandle_t<CSchemaType_Builtin>, m_pObj );

// tier0: the vector specialization the bridge walks the entity list through
// (`m_Size` at 0, `m_Memory.m_pMemory` at 8). `Base()` is the public member
// access that forces it; probe.cpp cannot use `m_Size` (protected, and this TU
// is compiled without the `#define protected public` trick the probe pass uses).
S2SDK_FORCE_SPEC_FIELD( CUtlVector_CEntityHandle, CUtlVector<CEntityHandle>, Base() );

// rendersystem: the reference-holding texture handle four IRenderDevice slots return by value
// (one binding pointer, but a user-provided copy constructor and destructor, so it is returned
// through memory). `GetBinding()` is the public accessor that forces the definition.
S2SDK_FORCE_SPEC_FIELD( CStrongHandle_InfoForResourceTypeCTextureBase, CStrongHandle<InfoForResourceTypeCTextureBase>, GetBinding() );

// metamod: the second upstream's interfaces (see the include block above).
// IKHook's 15 virtuals are all pure and it declares no destructor, so it is
// 15 slots on both ABIs; ISmmPlugin declares a virtual destructor right after
// GetApiVersion, which is two Itanium entries and one MSVC one.
S2SDK_FORCE_VTABLE( ISmmPlugin, Load );
S2SDK_FORCE_VTABLE( IMetamodListener, OnPluginLoad );
S2SDK_FORCE_VTABLE( ISmmAPI, GetDetourInterface );
S2SDK_FORCE_VTABLE( ISmmPluginManager, Query );
S2SDK_FORCE_VTABLE( IKHook, SetupHook );
