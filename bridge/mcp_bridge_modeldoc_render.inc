/* mcp_bridge_modeldoc_render.inc - modeldoc.render (m2.14, ModelDoc vision pass WP-B): the
   ModelDoc editor's main 3D viewport rendered offscreen to a PNG. Included by mcp_bridge.c right
   after mcp_bridge_pet_render.inc, so op_err, mem_ok, obj_is, logline/lognum, the ModelDoc
   lifecycle helpers (resolve_modeldoc, active_modeldoc, CModelDocEditorApp/Session,
   MDOFF_* / MDV_*) and the shared render helpers (toolscene_render_params,
   toolscene_render_png, tag_log/tag_lognum) are in scope (never duplicated here).

   Render facts (target/orch/plan.md section 0, sigs/signatures/modeldoc_editor.json, all [V]
   static, modeldoc_editor.dll build 2000930):

     app (resolve_modeldoc) + S2_OFF_CBaseEditorApp_m_pActiveSession (0x50)  -> CModelDocEditorSession  MDV_SESSION_VT
       session + S2_OFF_CModelDocEditorSession_m_pMainWnd (0x110)          -> CQModelDocEditorWnd         MDV_EDITORWND_VT
         mainwnd + MDOFF_MAINWND_SCENEWIDGET (0x1E0)    -> CQModelDocEditorSceneWidget MDV_SCENEWIDGET_VT
           scenewidget + MDOFF_SCENEWIDGET_TOOLWIDGET (0x90) -> CQModelDocToolSceneWidget MDV_TOOLWIDGET_VT
             toolwidget + MDOFF_TOOLWIDGET_SCENE (0x50)      -> CToolScene             MDV_TOOLSCENE_VT
             toolwidget + MDOFF_TOOLWIDGET_CAMERA (0x58)     -> CToolSceneCamera       MDV_SCENECAMERA_VT

   MDFN_TOOLSCENE_RENDER (RVA 0xB53770) is the module's own statically linked copy of
   CToolScene::Render; same body and signature as PETFN_TOOLSCENE_RENDER, so it is called exactly
   like pet.render: wnd = 0 (an offscreen window built from the camera), a RGBA8888 pixel buffer
   and vp {1,0,0,w,h,0,1}. The whole tail (sentinel buffer, Render, stats, crc, png, atomic
   write, result JSON) is the shared toolscene_render_png, so the wire result is pet.render's
   shape with the same bounds and the same -32020 reasons.

   The session owns the main window, so a render needs an active session (`no_session` /
   `bad_session` otherwise); the document itself is optional and is only reported as
   hasDocument, exactly as pet.render reports it.

   The camera class is only a CANDIDATE identity here (pet live-verified the same class in its own
   module): a mismatch is "bad_camera" and logs the observed vtable RVA so the RE can fix
   MDV_SCENECAMERA_VT (fail closed) - and every other hop logs its RVA the same way (m2.14 plan
   section 1). Out of scope by plan section 3: camera control, preview-window docks and any UI
   selection; the render uses the viewport's current camera. */

/* %s does not match its pinned vtable (candidate), observed: <rva> - one per bad hop, so a
   mismatch names the object and its actual class in the bridge log (m2.14 plan section 1). */
static void md_render_bad_hop(const char *hop, void *obj) {
    char why[160]; SB wb; sb_init(&wb, why, sizeof why);
    sb_add(&wb, hop); sb_add(&wb, " does not match its pinned vtable (candidate), observed:");
    tag_log("modeldoc.render", why);
    if (mem_ok(obj)) {
        char pfx[96]; SB pb; sb_init(&pb, pfx, sizeof pfx);
        sb_add(&pb, hop); sb_add(&pb, " vtable rva ");
        tag_lognum("modeldoc.render", pfx,
                   (u64)(*(u8 **)obj - (u8 *)g_modBase[PIN_MODELDOC]));
    }
}

/* app -> session -> main window -> scene widget -> tool widget -> scene + camera, every hop read
   after mem_ok/obj_is (identity, hop by hop, like pet_render_chain; a null hop is `no_*`, an
   identity mismatch is `bad_*`). 1 = ok; 0 = *reason is one of the modeldoc.render reasons
   (plan section 1). Unlike active_modeldoc the document is not required: the main window hangs
   off the session. */
static int modeldoc_render_chain(void **sceneOut, void **cameraOut, const char **reason) {
    *reason = "ok";
    if (!mod_ok(PIN_MODELDOC)) {
        *reason = (g_modState[PIN_MODELDOC] == MS_MISMATCH) ? "modeldoc_mismatch"
                : (g_modState[PIN_MODELDOC] == MS_VERIFYING) ? "modeldoc_verifying"
                : "modeldoc_not_loaded";
        return 0;
    }
    CModelDocEditorApp *app = resolve_modeldoc();
    if (!app) { *reason = "modeldoc_unavailable"; return 0; }
    CModelDocEditorSession *session = CModelDocEditorApp_ActiveSession(app);
    if (!session) { *reason = "no_session"; return 0; }
    if (!obj_is(session, PIN_MODELDOC, MDV_SESSION_VT)) {
        md_render_bad_hop("session", session); *reason = "bad_session"; return 0;
    }
    void *wnd = *(void **)((u8 *)session + S2_OFF_CModelDocEditorSession_m_pMainWnd);
    if (!wnd) { *reason = "no_main_window"; return 0; }
    if (!obj_is(wnd, PIN_MODELDOC, MDV_EDITORWND_VT)) {
        md_render_bad_hop("main_window", wnd); *reason = "bad_main_window"; return 0;
    }
    void *widget = *(void **)((u8 *)wnd + MDOFF_MAINWND_SCENEWIDGET);
    if (!widget) { *reason = "no_scene_widget"; return 0; }
    if (!obj_is(widget, PIN_MODELDOC, MDV_SCENEWIDGET_VT)) {
        md_render_bad_hop("scene_widget", widget); *reason = "bad_scene_widget"; return 0;
    }
    void *tool = *(void **)((u8 *)widget + MDOFF_SCENEWIDGET_TOOLWIDGET);
    if (!tool) { *reason = "no_tool_widget"; return 0; }
    if (!obj_is(tool, PIN_MODELDOC, MDV_TOOLWIDGET_VT)) {
        md_render_bad_hop("tool_widget", tool); *reason = "bad_tool_widget"; return 0;
    }
    void *scene = *(void **)((u8 *)tool + MDOFF_TOOLWIDGET_SCENE);
    if (!scene) { *reason = "no_scene"; return 0; }
    if (!obj_is(scene, PIN_MODELDOC, MDV_TOOLSCENE_VT)) {
        md_render_bad_hop("scene", scene); *reason = "bad_scene"; return 0;
    }
    void *camera = *(void **)((u8 *)tool + MDOFF_TOOLWIDGET_CAMERA);
    if (!camera) { *reason = "no_camera"; return 0; }
    if (!obj_is(camera, PIN_MODELDOC, MDV_SCENECAMERA_VT)) {
        md_render_bad_hop("camera", camera); *reason = "bad_camera"; return 0;
    }
    *sceneOut = scene;
    *cameraOut = camera;
    return 1;
}

static int op_modeldoc_render(const char *params, SB *out, OpErr *e) {
    /* parameters first: -32602 before any object is touched (pet.render's exact contract) */
    u32 w, h;
    char stem[80];
    if (!toolscene_render_params(params, &w, &h, stem, sizeof stem, e)) return 0;

    void *scene, *camera;
    const char *reason = "?";
    if (!modeldoc_render_chain(&scene, &camera, &reason)) {
        op_err(e, -32019, "NoViewport", reason);
        return 0;
    }
    const char *docReason = "?";
    int hasDocument = active_modeldoc(&docReason) ? 1 : 0;   /* allowed without a document */

    ToolSceneRenderFn render =
        (ToolSceneRenderFn)((u8 *)g_modBase[PIN_MODELDOC] + MDFN_TOOLSCENE_RENDER);   /* pinned module (resolve_modeldoc checked) */
    return toolscene_render_png("modeldoc.render", render, scene, camera, hasDocument, w, h,
                                stem, out, e);
}
