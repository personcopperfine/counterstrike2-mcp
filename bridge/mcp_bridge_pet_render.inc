/* mcp_bridge_pet_render.inc - the "viewport render-to-memory -> PNG" ops: pet.render (m2.13,
   WP-B2) and modeldoc.render (m2.14, ModelDoc vision pass WP-B). Included by mcp_bridge.c right
   after mcp_bridge_pet.inc, so resolve_pet, active_pet, render_name, png_encode,
   write_file_atomic, crc_init/crc_update and VR_SENTINEL are in scope (never duplicated here).

   Both ops are the same call on the same statically linked tools-lib function, one copy per
   module: CToolScene::Render(scene, camera, IToolSceneWindow *wnd|0, swapchain|0,
   const RenderViewport_t *vp (w @+12, h @+16), u8 *pixels|0, int imageFormat). With wnd = 0 it
   builds a COffscreenToolSceneWindow from the camera, runs the scene system's Begin/Finish/Wait
   bracket itself and reads the colour target back with the render device's ReadTexturePixels
   into pixels (row pitch w * bpp). Image formats (pet table 0x18082F0C0): 0 = RGBA8888 (4 B/px),
   2 = RGB888 (3 B/px, what the widget paints). The pixels are display-ready (the widget shows
   them as a QImage unchanged), so the stats loop applies NO sRGB LUT here, unlike view.render.
   Rendering outside the widget's paint from the main-thread drain mirrors world.render: the
   whole Begin/Finish/Wait bracket happens inside the one call, so no engine frame can start in
   between (the same reasoning as op_world_render's bracket comment).

   m2.14 (ModelDoc vision pass WP-B) factored everything after the chain - parameter validation,
   the sentinel buffer, the Render call, the convert + stats pass, crc, png encode, the atomic
   write and the result JSON - into the two shared helpers below, so modeldoc.render reuses
   pet.render's exact bounds, error messages, pixel maths and wire shape. The log prefix comes
   from `tag`, so pet.render's log lines are unchanged.

   The camera class is only a CANDIDATE identity in both chains: a mismatch is the specific
   "bad_camera" error AND logs the observed vtable RVA so the RE can fix PETV_SCENECAMERA_VT in
   pet.json (fail closed; live step 18). The ModelDoc chain logs every bad hop's RVA the same
   way (m2.14 plan section 1).

   Unverified (review 2026-10-13): the widget's own paint (0x1803E34F0) wraps Render in a render
   context acquire/release on the render device and sets two scene flags first; this call is bare,
   like world.render's. If pet.render crashes or returns all-sentinel pixels, suspect that bracket
   first. Whether the wnd==0 offscreen window is freed per call is also unchecked (a per-render
   leak at worst). */

typedef void (*ToolSceneRenderFn)(void *scene, void *camera, void *wnd, void *swapchain,
                                  const S2_RenderViewport_t *vp, u8 *pixels, int imageFormat);

/* one log line: `tag` + ": " + `what` ("pet.render: begin"). tag_lognum appends the decimal
   value (lognum truncates a prefix at 56 chars; both tags are 12). */
static void tag_log(const char *tag, const char *what) {
    char buf[160];
    SB b; sb_init(&b, buf, sizeof buf);
    sb_add(&b, tag); sb_add(&b, ": "); sb_add(&b, what);
    logline(buf);
}
static void tag_lognum(const char *tag, const char *what, u64 v) {
    char buf[160];
    SB b; sb_init(&b, buf, sizeof buf);
    sb_add(&b, tag); sb_add(&b, ": "); sb_add(&b, what);
    lognum(buf, v);
}

/* pet.render / modeldoc.render parameter contract (m2.14 plan section 1; identical for both):
   width/height default 1024x768, each side 16..4096, at most 8388608 pixels; `name` follows
   render_name. Parameters are validated before any object is touched, so a bad request can
   never reach the game. */
static int toolscene_render_params(const char *params, u32 *wOut, u32 *hOut,
                                   char *stem, u32 stemCap, OpErr *e) {
    long long lw = 1024, lh = 768;
    if (params) { json_int(params, "width", &lw); json_int(params, "height", &lh); }
    if (lw < 16 || lh < 16 || lw > VR_MAX_SIDE || lh > VR_MAX_SIDE || (u64)lw * (u64)lh > VR_MAX_PIXELS) {
        op_err(e, -32602, "InvalidParams", "width/height: 16..4096 each, at most 8388608 pixels");
        return 0;
    }
    if (!render_name(params, stem, stemCap)) {
        op_err(e, -32602, "InvalidParams", "name: [A-Za-z0-9._-], 1..64 chars, no leading dot");
        return 0;
    }
    *wOut = (u32)lw;
    *hOut = (u32)lh;
    return 1;
}

/* the shared tail of pet.render / modeldoc.render: the caller has already validated the
   parameters and resolved the object chain (scene, camera) and the document flag. `tag` is the
   log prefix and `renderFn` the module's own CToolScene::Render. Returns 1 with the result JSON
   in `out`, 0 with *e set. The wire result is byte-identical for both ops. */
static int toolscene_render_png(const char *tag, ToolSceneRenderFn renderFn, void *scene,
                                void *camera, int hasDocument, u32 w, u32 h, const char *stem,
                                SB *out, OpErr *e) {
    u64 npx = (u64)w * h;
    u8 *px = (u8 *)HeapAlloc(GetProcessHeap(), 0, npx * 4);
    u8 *raw = (u8 *)HeapAlloc(GetProcessHeap(), 0, (u64)h * (1 + 3ull * w));
    if (!px || !raw) {
        if (px) HeapFree(GetProcessHeap(), 0, px);
        if (raw) HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "out of memory");
        return 0;
    }
    for (u64 i = 0; i < npx; i++) ((u32 *)px)[i] = VR_SENTINEL;   /* detects a readback that never ran */

    S2_RenderViewport_t vp = { 1, 0, 0, (int)w, (int)h, 0.0f, 1.0f };   /* generated RenderViewport_t (28 B) */
    u64 t0 = GetTickCount64();
    tag_log(tag, "begin");
    renderFn(scene, camera, 0 /* wnd = 0: offscreen window */, 0 /* swapchain */, &vp, px, 0 /* RGBA8888 */);
    tag_log(tag, "finish");

    /* convert + stats; the pixels are display-ready (no sRGB LUT, unlike view.render) */
    u64 untouched = 0, lumaSum = 0, nonBlack = 0;
    u32 lumaMin = 255, lumaMax = 0;
    for (u32 y = 0; y < h; y++) {
        u8 *row = raw + (u64)y * (1 + 3ull * w);
        row[0] = 0;
        for (u32 x = 0; x < w; x++) {
            const u8 *s = px + ((u64)y * w + x) * 4;
            u64 bi = ((u64)y * w + x) * 4;
            u32 sent = 1;
            for (u32 k = 0; k < 4; k++) {
                if (s[k] != (u8)(VR_SENTINEL >> (8 * ((bi + k) & 3)))) { sent = 0; break; }
            }
            untouched += sent;
            u8 r = s[0], g = s[1], bl = s[2];
            row[1 + 3 * x] = r; row[2 + 3 * x] = g; row[3 + 3 * x] = bl;
            u32 luma = (54u * r + 183u * g + 19u * bl) >> 8;
            lumaSum += luma;
            if (luma < lumaMin) lumaMin = luma;
            if (luma > lumaMax) lumaMax = luma;
            if (luma > 8) nonBlack++;
        }
    }
    HeapFree(GetProcessHeap(), 0, px);
    if (untouched == npx) {
        HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "readback did not run (no pixels written)");
        return 0;
    }
    crc_init();
    u32 pixCrc = crc_update(0xFFFFFFFFu, raw, (u64)h * (1 + 3ull * w)) ^ 0xFFFFFFFFu;
    u64 pngLen = 0;
    u8 *png = png_encode(raw, w, h, &pngLen);
    HeapFree(GetProcessHeap(), 0, raw);
    if (!png) { op_err(e, -32020, "RenderFailed", "png encode: out of memory"); return 0; }

    CreateDirectoryA("Z:\\app\\renders", 0);
    char path[200];
    SB ps; sb_init(&ps, path, sizeof path);
    sb_add(&ps, "Z:\\app\\renders\\"); sb_add(&ps, stem); sb_add(&ps, ".png");
    int wrote = !ps.ovf && write_file_atomic(path, png, pngLen);
    HeapFree(GetProcessHeap(), 0, png);
    u64 t2 = GetTickCount64();
    if (!wrote) { op_err(e, -32020, "RenderFailed", "could not write Z:\\app\\renders"); return 0; }
    tag_log(tag, "wrote");
    logline(path);

    static const char *hex = "0123456789abcdef";
    char fname[96];
    SB fs; sb_init(&fs, fname, sizeof fname);
    sb_add(&fs, stem); sb_add(&fs, ".png");
    sb_add(out, "{\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"file\":"); sb_addjson(out, fname);
    sb_add(out, ",\"width\":"); sb_addnum(out, w);
    sb_add(out, ",\"height\":"); sb_addnum(out, h);
    sb_add(out, ",\"bytes\":"); sb_addnum(out, pngLen);
    sb_add(out, ",\"format\":\"RGBA8888\"");
    sb_add(out, ",\"hasDocument\":"); sb_add(out, hasDocument ? "true" : "false");
    sb_add(out, ",\"stats\":{\"untouchedPixels\":"); sb_addnum(out, untouched);
    sb_add(out, ",\"nonBlackPixels\":"); sb_addnum(out, nonBlack);
    sb_add(out, ",\"meanLuma\":"); sb_addf(out, (float)((double)lumaSum / (double)npx));
    sb_add(out, ",\"minLuma\":"); sb_addnum(out, lumaMin);
    sb_add(out, ",\"maxLuma\":"); sb_addnum(out, lumaMax);
    sb_add(out, ",\"crc32\":\"");
    for (int i = 7; i >= 0; i--) sb_putc(out, hex[(pixCrc >> (4 * i)) & 15]);
    sb_add(out, "\"}");
    sb_add(out, ",\"durationMs\":"); sb_addnum(out, t2 - t0);
    sb_add(out, "}");
    return 1;
}

/* pet app -> main window -> preview -> scene widget -> scene + camera, every hop read after
   mem_ok/obj_is (identity, hop by hop, like resolve_pet/active_pet). 1 = ok; 0 = *reason is one
   of the pet.render reasons (plan section 0.1). */
static int pet_render_chain(void **sceneOut, void **cameraOut, const char **reason) {
    *reason = "ok";
    CPetApp *app = resolve_pet();
    if (!app) { *reason = "pet_unavailable"; return 0; }
    void *wnd = app->mainWnd;
    if (!wnd) { *reason = "no_main_window"; return 0; }
    if (!obj_is(wnd, PIN_PET, PETV_MAINWND_VT)) { *reason = "bad_main_window"; return 0; }
    void *preview = *(void **)((u8 *)wnd + PETOFF_MAINWND_PREVIEW);
    if (!preview) { *reason = "no_preview"; return 0; }
    if (!obj_is(preview, PIN_PET, PETV_PREVIEW_VT)) { *reason = "bad_preview"; return 0; }
    void *widget = *(void **)((u8 *)preview + PETOFF_PREVIEW_SCENEWIDGET);
    if (!widget) { *reason = "no_scene_widget"; return 0; }
    if (!obj_is(widget, PIN_PET, PETV_SCENEWIDGET_VT)) { *reason = "bad_scene_widget"; return 0; }
    void *scene = *(void **)((u8 *)widget + PETOFF_SCENEWIDGET_SCENE);
    if (!scene) { *reason = "no_scene"; return 0; }
    if (!obj_is(scene, PIN_PET, PETV_TOOLSCENE_VT)) { *reason = "bad_scene"; return 0; }
    void *camera = *(void **)((u8 *)widget + PETOFF_SCENEWIDGET_CAMERA);
    if (!camera) { *reason = "no_camera"; return 0; }
    if (!obj_is(camera, PIN_PET, PETV_SCENECAMERA_VT)) {
        logline("pet.render: camera does not match PETV_SCENECAMERA_VT (candidate), observed:");
        if (mem_ok(camera))
            lognum("pet.render: camera vtable rva ",
                   (u64)(*(u8 **)camera - (u8 *)g_modBase[PIN_PET]));
        *reason = "bad_camera";
        return 0;
    }
    *sceneOut = scene;
    *cameraOut = camera;
    return 1;
}

static int op_pet_render(const char *params, SB *out, OpErr *e) {
    /* parameters first: -32602 before any object is touched */
    u32 w, h;
    char stem[80];
    if (!toolscene_render_params(params, &w, &h, stem, sizeof stem, e)) return 0;

    void *scene, *camera;
    const char *reason = "?";
    if (!pet_render_chain(&scene, &camera, &reason)) {
        op_err(e, -32019, "NoViewport", reason);
        return 0;
    }
    const char *docReason = "?";
    int hasDocument = active_pet(&docReason) ? 1 : 0;   /* allowed without a document */

    ToolSceneRenderFn render =
        (ToolSceneRenderFn)((u8 *)g_modBase[PIN_PET] + PETFN_TOOLSCENE_RENDER);   /* pinned module (resolve_pet checked) */
    return toolscene_render_png("pet.render", render, scene, camera, hasDocument, w, h, stem, out, e);
}
