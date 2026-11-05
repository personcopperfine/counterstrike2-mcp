# Reviewer-facing notes for the upstream PR description(s)

- Wend4r's restore-* branches are largely AI-authored; several names there are credited to s&box
  debug info (not one of "the games"). Disclose in the PR description.
- Unknown groups past 9 entries use plain decimal concatenation (unk1010, unk14010).
- Scenesystem is mostly a client module; tool classes (toolframework2, hammer, pet, modeldoc) and
  CCSGOInput are outside upstream's "mainly server plugins" scope. Split per the user's later plan.
- New single-header folders: public/toolframework2/, public/soundsystem/ (no path strings found).
- Verified on CS2 2000930 win64 + linuxsteamrt64 only; Dota/Deadlock not checked (user decision).
- boeing666 co-authored iscenesystem.h / isceneutils.h / itestscriptmgr.h on Wend4r's fork.
- engine2 interfaces (IRenderService, IInputService, IVEngineClient2) are client-side but the Linux
  dedicated-server libengine2.so carries all three vtables.
- Wend4r restore-* commits authored "Claude <noreply@anthropic.com>" (committer Vladimir Ezhikov);
  credit uses Wend4r's own noreply address.
- Vector4D has a user-declared copy ctor, so it can't be returned in registers on Linux like the
  engine's IRenderDevice slot 92 does; placeholder used there. Upstream may want the ctor removed.
- IRenderDevice: Wend4r's enum names (RenderDeviceAPI_t etc.) are not in any binary; plain ints.
- IToolFramework2::MakeVConsoleForeground is declared void; the VConComm method it forwards to
  returns bool (spec chose void deliberately). Maintainers may prefer bool.
- IRenderDevice slots 51/53/54/55 return CStrongHandle< InfoForResourceTypeCTextureBase > (new template in
  resourcesystem/resourcehandle.h, the engine's reference-holding handle: add-ref on copy, DeleteResource on the last release).
  Slot 92 returns RenderDeviceUnkVector4_t (four floats) because upstream Vector4D has a user-declared copy ctor.
- ImageFormat lives in a new public/bitmap/ folder (Source 1 path + src\bitmap strings in the binaries); it is the
  CS2 enum (from value 7 on it differs from Source 1). HRenderTexture moved from scenesystem/iscenesystem.h to
  rendersystem/rendersystemtypes.h. The bridge's slot-168 name PreferDepthFormat48 is IsUsingReverseDepth.
- The engine's own 4-float type appears as Vec4D<float> in particles.dll (IRenderDevice slot 92 placeholder).
- EAttributeDataType: CS2 has a fifth type at 4 (only the BONE_INDICES attribute uses it), added as ATTRDATATYPE_UNK001 before ATTRDATATYPE_COUNT (now 5); ATTRDATATYPE_4V's "FourVectors" comment is left as is (the engine's element size for it is 12 bytes).
- ButtonCode_t: the CS2 enum has KEY_LESS at 53 and 209 more keys after KEY_SCROLLLOCKTOGGLE (KEY_LAST 316, MOUSE_LEFT 317, JOYSTICK_FIRST 324); the engine's EXECUTE key is KEY_EXECUTE_KEY because winnt.h defines KEY_EXECUTE.
- Pre-existing upstream, not ours: ILoopMode::unk001 (iengineservicemgr.h) follows named methods, so the unk numbering rule would make it unk101.
- CBasePetNode::GetClassName collides with the windows.h GetClassName -> GetClassNameA macro (handled like CreateEvent; the C slot macro is S2_SLOT_CBasePetNode_GetClassNameA). It is a behaviour name, not an engine name; rename (e.g. GetSchemaClassName) if reviewers object.
- Some unknown names repeat across two bases (unk101 in CBaseEditorApp, m_unk001 in CPetSession); unknown numbering is per class, so same-named unknowns in different bases are expected.
- IAssetFingerprintListener is declared only as CModelDocEditorSession's secondary base; nothing in the SDK registers one.
- CToolNodeTypeName is not reachable from any SDK interface and its folder (public/toolutils2) has no file-path evidence; fallback is to drop it and make MDOFF_NODETYPE_NAME not-SDK.
- CModelDocEditorApp sizeof 0x418 is bounded exactly: settings member at +0x1F8 writes up to +0x413, the next referenced static's guard sits at +0x418, and a sibling static of the same settings type is 0x220 apart.
- CModelDocEditorSession inherits unk101 / m_unk001 from two bases (same as CPetSession).
- Hammer: listener interfaces with no registration path in the SDK ("a listener interface needs a way to register it"): IVDataLookupListener, IHammerMeshChangeListener, IBindableCommandListener, IAssetFileListener, ITagVisualizationListener, IShaderReloadListener, IBoundingBoxChangeListener. They are declared only because they are bases of declared classes.
- CMapDoc and CEditScopeStack have non-pure, undefined `override`s, needed to embed CMapDoc by value in CMapDocSession. Users must not construct them.
- IDataModel derives from `CBaseAppSystem< IAppSystem >` (upstream's implementation template), not `IAppSystem` as other interfaces do; the RTTI class hierarchy descriptor backs it. It is the first such use.
- DmElementHandle_t, DmAttributeHandle_t and DmFileId_t carry `: uint32` (the engine treats them as 32-bit unsigned: a 23-bit index plus a 9-bit serial; 0xffffffff would be -1 under MSVC otherwise).
- IDataModel time functions: GetFileModificationUTCTime and UTCTimeToString's fileTime are 32-bit `int`, GetCurrentUTCTime returns a 64-bit `time_t` (it tail-calls Plat_GetTime); the asymmetry is in the engine.
- hammereditorsession.h keeps `class CMapViewMgr;` (mapviewmgr.h comes after it in the series); idatamodel.h keeps `CUtlBuffer` / `KeyValues` forward declarations (utlbuffer.h is a long include chain).
- CMapViewMgr member numbering: its three listener bases have no members, so its members are m_unk001 / m_unk101.
