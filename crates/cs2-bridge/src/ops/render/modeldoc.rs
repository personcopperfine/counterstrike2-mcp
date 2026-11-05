//! `modeldoc.render`: the ModelDoc editor's main 3D viewport rendered offscreen to a PNG
//! (`mcp_bridge_modeldoc_render.inc`).
//!
//! The chain is `CModelDocEditorApp` -> session -> main window -> scene widget -> tool widget ->
//! `(scene, camera)` (`modeldoc_render_chain`, `:54-97`); the render, stats, PNG and reply are
//! the shared [`super::toolscene`] tail, so this row's wire result is `pet.render`'s shape with
//! the same bounds and the same `-32020` reasons.
//!
//! The session owns the main window, so a render needs an active session (`no_session` /
//! `bad_session` otherwise); the document itself is optional and is only reported as
//! `hasDocument`, exactly as `pet.render` reports it (`:111-112`).
//!
//! The camera class is only a CANDIDATE identity here; every bad hop logs the observed vtable RVA
//! (`md_render_bad_hop`, `:37-47`), so a mismatch names the object and its actual class in the
//! bridge log (fail closed).
//!
//! Parameters are validated **before** any object is touched (`:100-103`).

use core::ffi::c_void;

use crate::entity_json::Sb;
#[cfg(windows)]
use crate::generated::pins;
#[cfg(windows)]
use crate::modules;
#[cfg(windows)]
use crate::ops::ident::obj_is;
use crate::ops::OpErr;
use crate::queue::Request;

use super::toolscene::{render_png, ToolSceneRenderFn};
#[cfg(windows)]
use super::toolscene::{tag_log, tag_lognum};
use super::toolscene_params;

/// `modeldoc.render`'s arm goes through `op_modeldoc` and therefore the modeldoc arm's 256 KiB
/// result buffer and its overflow answer (`mcp_bridge.c:6205-6219`,
/// `mcp_bridge_modeldoc_render.inc:99-118`).
pub(super) fn handler(request: &Request) -> String {
    let mut buf = vec![0u8; crate::ops::modeldoc::RESULT_BUF];
    let mut out = Sb::new(&mut buf);
    let outcome = op_modeldoc_render(request.params.as_deref(), &mut out);
    crate::ops::finish(
        &request.id,
        outcome,
        &out,
        crate::ops::modeldoc::MODELDOC_OVERFLOW,
    )
}

/// `op_modeldoc_render` (`mcp_bridge_modeldoc_render.inc:99-118`).
fn op_modeldoc_render(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    // :100-103: parameters first - -32602 before any object is touched (pet.render's contract).
    let (w, h, stem) = toolscene_params(params)?;

    let (scene, camera) =
        modeldoc_chain().map_err(|reason| OpErr::new(-32019, "NoViewport", reason))?; // :106-110
    // `active_modeldoc(&docReason) ? 1 : 0` - allowed without a document (`:112`).
    let has_document = crate::ops::modeldoc::active_modeldoc_with(crate::ops::modeldoc::game()).is_ok();
    #[cfg(windows)]
    let render = render_fn();
    #[cfg(not(windows))]
    let render: ToolSceneRenderFn = unreachable_render;
    render_png("modeldoc.render", render, scene, camera, has_document, w, h, &stem, out) // :116-117
}

/// The absent-platform stand-in for `CToolScene::Render` (never called: the chain refuses first).
#[cfg(not(windows))]
unsafe extern "C" fn unreachable_render(
    _scene: *mut c_void,
    _camera: *mut c_void,
    _wnd: *mut c_void,
    _swapchain: *mut c_void,
    _vp: *const s2sdk_sys::generated::RenderViewport_t,
    _pixels: *mut u8,
    _image_format: i32,
) {
    unreachable!("modeldoc.render: no modeldoc_editor.dll half on this platform")
}

/// `(u8 *)g_modBase[PIN_MODELDOC] + MDFN_TOOLSCENE_RENDER` (`:114-115`): the module's own
/// statically linked `CToolScene::Render`.
#[cfg(windows)]
fn render_fn() -> ToolSceneRenderFn {
    let address = modules::live().base(pins::PIN_MODELDOC) as usize
        + pins::MDFN_TOOLSCENE_RENDER.value as usize;
    // SAFETY: `resolve_modeldoc` proved the module verified (and pinned it), and the address is
    // the pinned `CToolScene::Render` RVA whose prologue the module gate checked.
    unsafe { core::mem::transmute::<usize, ToolSceneRenderFn>(address) }
}

/// `%s does not match its pinned vtable (candidate), observed: <rva>` (`:37-47`).
#[cfg(windows)]
fn bad_hop(hop: &str, object: usize) {
    tag_log(
        "modeldoc.render",
        &format!("{hop} does not match its pinned vtable (candidate), observed:"),
    );
    if crate::ops::ident::mem_ok(object as *const c_void) {
        // SAFETY: `mem_ok` just proved the vptr word readable; the C reads it the same way.
        let vptr = unsafe { (object as *const u64).read_unaligned() };
        let base = modules::live().base(pins::PIN_MODELDOC);
        tag_lognum(
            "modeldoc.render",
            &format!("{hop} vtable rva "),
            vptr.wrapping_sub(base),
        );
    }
}

/// `modeldoc_render_chain` (`mcp_bridge_modeldoc_render.inc:54-97`).
#[cfg(windows)]
fn modeldoc_chain() -> Result<(*mut c_void, *mut c_void), &'static str> {
    use core::ffi::c_void as void;

    // :56-61: the module gate's own reasons, before the app hop.
    let md = crate::ops::modeldoc::game();
    if let Some(reason) = md.module_reason() {
        return Err(reason);
    }
    let app = md.app().ok_or("modeldoc_unavailable")?; // :62-63
    // `CModelDocEditorApp_ActiveSession` (`:65`): the modeldoc lane's `Chain::session`, the same
    // generated `CBaseEditorApp::m_pActiveSession` read (0 = no session).
    let session = md.session(app).ok_or("no_session")?;
    let session = session as *mut void;
    if !obj_is(session, pins::PIN_MODELDOC, pins::MDV_SESSION_VT.value as u32) {
        bad_hop("session", session as usize);
        return Err("bad_session"); // :66-68
    }
    // SAFETY: `session` passed `obj_is(MDV_SESSION_VT)`; `m_pMainWnd` is the generated member the
    // C reaches with `S2_OFF_CModelDocEditorSession_m_pMainWnd`.
    let wnd = unsafe {
        (*((session) as *const s2sdk_sys::generated::CModelDocEditorSession)).m_pMainWnd
    };
    if wnd.is_null() {
        return Err("no_main_window"); // :70
    }
    if !obj_is(wnd, pins::PIN_MODELDOC, pins::MDV_EDITORWND_VT.value as u32) {
        bad_hop("main_window", wnd as usize);
        return Err("bad_main_window"); // :71-73
    }
    // SAFETY: `wnd` passed `obj_is(MDV_EDITORWND_VT)`; the pinned member offset.
    let widget = unsafe {
        ((wnd as *const u8).add(pins::MDOFF_MAINWND_SCENEWIDGET.value as usize)
            as *const *mut void)
            .read_unaligned()
    };
    if widget.is_null() {
        return Err("no_scene_widget"); // :75
    }
    if !obj_is(widget, pins::PIN_MODELDOC, pins::MDV_SCENEWIDGET_VT.value as u32) {
        bad_hop("scene_widget", widget as usize);
        return Err("bad_scene_widget"); // :76-78
    }
    // SAFETY: as above; `MDOFF_SCENEWIDGET_TOOLWIDGET`.
    let tool = unsafe {
        ((widget as *const u8).add(pins::MDOFF_SCENEWIDGET_TOOLWIDGET.value as usize)
            as *const *mut void)
            .read_unaligned()
    };
    if tool.is_null() {
        return Err("no_tool_widget"); // :80
    }
    if !obj_is(tool, pins::PIN_MODELDOC, pins::MDV_TOOLWIDGET_VT.value as u32) {
        bad_hop("tool_widget", tool as usize);
        return Err("bad_tool_widget"); // :81-83
    }
    // SAFETY: as above; `MDOFF_TOOLWIDGET_SCENE`.
    let scene = unsafe {
        ((tool as *const u8).add(pins::MDOFF_TOOLWIDGET_SCENE.value as usize) as *const *mut void)
            .read_unaligned()
    };
    if scene.is_null() {
        return Err("no_scene"); // :85
    }
    if !obj_is(scene, pins::PIN_MODELDOC, pins::MDV_TOOLSCENE_VT.value as u32) {
        bad_hop("scene", scene as usize);
        return Err("bad_scene"); // :86-88
    }
    // SAFETY: as above; `MDOFF_TOOLWIDGET_CAMERA`.
    let camera = unsafe {
        ((tool as *const u8).add(pins::MDOFF_TOOLWIDGET_CAMERA.value as usize) as *const *mut void)
            .read_unaligned()
    };
    if camera.is_null() {
        return Err("no_camera"); // :90
    }
    if !obj_is(camera, pins::PIN_MODELDOC, pins::MDV_SCENECAMERA_VT.value as u32) {
        bad_hop("camera", camera as usize);
        return Err("bad_camera"); // :91-93
    }
    Ok((scene, camera))
}

/// No modeldoc_editor.dll on this target: the module state is `not_loaded`, the C's own reason
/// (the modeldoc lane's `Chain::module_reason`, `crates/cs2-bridge/src/ops/modeldoc.rs:979-982`).
#[cfg(not(windows))]
fn modeldoc_chain() -> Result<(*mut c_void, *mut c_void), &'static str> {
    Err(crate::ops::modeldoc::game()
        .module_reason()
        .unwrap_or("modeldoc_not_loaded"))
}
