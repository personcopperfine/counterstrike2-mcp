//! `pet.render`: the particle editor's preview viewport rendered offscreen to a PNG
//! (`mcp_bridge_pet_render.inc:169-221`).
//!
//! The chain is `CPetApp` -> main window -> preview -> scene widget -> `(scene, camera)`, every
//! hop identity-checked (`pet_render_chain`, `:172-201`); the render itself, the stats, the PNG
//! and the reply are the shared [`super::toolscene`] tail, so the wire result matches
//! `modeldoc.render`'s byte for byte.
//!
//! Parameters are validated **before** any object is touched (`:203-207`), so a bad request can
//! never reach the game.
//!
//! The camera class is only a CANDIDATE identity: a mismatch is the specific `bad_camera` error
//! and logs the observed vtable RVA so `PETV_SCENECAMERA_VT` can be corrected in `pet.json`
//! (fail closed, `:190-197`).

use core::ffi::c_void;

use crate::entity_json::Sb;
#[cfg(windows)]
use crate::generated::pins;
#[cfg(windows)]
use crate::log;
#[cfg(windows)]
use crate::ops::ident::obj_is;
use crate::ops::OpErr;
use crate::queue::Request;

use super::toolscene::{render_png, ToolSceneRenderFn};
#[cfg(windows)]
use super::toolscene::tag_lognum;
use super::{finish_pet_sb, toolscene_params};

/// `pet.render`'s arm: the C's `pet.*` tail runs every `pet.*` reply through
/// `op_pet` -> `op_pet_render` (`mcp_bridge.c:6222-6231`,
/// `mcp_bridge_pet_render.inc:203-220`).
pub(super) fn handler(request: &Request) -> String {
    // `static char rbuf[MAX_LINE - 2048]` (`mcp_bridge.c:6222`).
    let mut buf = vec![0u8; crate::control::MAX_LINE as usize - 2048];
    let mut out = Sb::new(&mut buf);
    let outcome = op_pet_render(request.params.as_deref(), &mut out);
    finish_pet_sb(&request.id, outcome, &out)
}

/// `op_pet_render` (`mcp_bridge_pet_render.inc:203-221`).
fn op_pet_render(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    // :204-207: parameters first - -32602 before any object is touched.
    let (w, h, stem) = toolscene_params(params)?;

    let (scene, camera) = pet_chain().map_err(|reason| OpErr::new(-32019, "NoViewport", reason))?; // :211-214
    // `active_pet(&docReason) ? 1 : 0` - allowed without a document (`:216`).
    let has_document = pet_active();
    #[cfg(windows)]
    let render = render_fn();
    #[cfg(not(windows))]
    let render: ToolSceneRenderFn = unreachable_render;
    render_png("pet.render", render, scene, camera, has_document, w, h, &stem, out) // :218-220
}

/// `active_pet(&docReason) ? 1 : 0` (`mcp_bridge_pet_render.inc:216`).
fn pet_active() -> bool {
    crate::ops::pet::game::platform::active_session().is_ok()
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
    unreachable!("pet.render: no pet.dll half on this platform")
}

/// `(u8 *)g_modBase[PIN_PET] + PETFN_TOOLSCENE_RENDER` (`:218-219`).
#[cfg(windows)]
fn render_fn() -> ToolSceneRenderFn {
    let address = crate::modules::live().base(pins::PIN_PET) as usize
        + pins::PETFN_TOOLSCENE_RENDER.value as usize;
    // SAFETY: `resolve_pet` proved the module verified (and pinned it), and the address is the
    // pinned `CToolScene::Render` RVA whose prologue the module gate checked.
    unsafe { core::mem::transmute::<usize, ToolSceneRenderFn>(address) }
}

/// `pet_render_chain` (`mcp_bridge_pet_render.inc:172-201`): every hop read after its identity
/// check, hop by hop.
#[cfg(windows)]
fn pet_chain() -> Result<(*mut c_void, *mut c_void), &'static str> {
    use core::mem::size_of;

    use s2sdk_sys::generated::CBaseEditorSharedWindowApp;

    /// `S2_FIELD(CPetApp, mainWnd, S2_SIZEOF_CBaseEditorSharedWindowApp)`
    /// (`s2sdk.h:622-626`).
    const APP_MAIN_WND: usize = size_of::<CBaseEditorSharedWindowApp>();
    const _: () = assert!(APP_MAIN_WND == 488);

    let app = crate::ops::pet::game::platform::resolve_pet().ok_or("pet_unavailable")?; // :174-175
    // SAFETY: `resolve_pet` returned the identity-checked `CPetApp` (pinned module) and the
    // member offset is the generated base size the C names (`s2sdk.h:626`); the read is
    // the C's `app->mainWnd`.
    let wnd = unsafe { ((app as *const u8).add(APP_MAIN_WND) as *const *mut c_void).read_unaligned() };
    if wnd.is_null() {
        return Err("no_main_window"); // :177
    }
    if !obj_is(wnd, pins::PIN_PET, pins::PETV_MAINWND_VT.value as u32) {
        return Err("bad_main_window"); // :178
    }
    // SAFETY: `wnd` passed `obj_is(PETV_MAINWND_VT)`; `PETOFF_MAINWND_PREVIEW` is the pinned
    // member offset the C names.
    let preview = unsafe {
        ((wnd as *const u8).add(pins::PETOFF_MAINWND_PREVIEW.value as usize) as *const *mut c_void)
            .read_unaligned()
    };
    if preview.is_null() {
        return Err("no_preview"); // :180
    }
    if !obj_is(preview, pins::PIN_PET, pins::PETV_PREVIEW_VT.value as u32) {
        return Err("bad_preview"); // :181
    }
    // SAFETY: as above; `PETOFF_PREVIEW_SCENEWIDGET`.
    let widget = unsafe {
        ((preview as *const u8).add(pins::PETOFF_PREVIEW_SCENEWIDGET.value as usize)
            as *const *mut c_void)
            .read_unaligned()
    };
    if widget.is_null() {
        return Err("no_scene_widget"); // :183
    }
    if !obj_is(widget, pins::PIN_PET, pins::PETV_SCENEWIDGET_VT.value as u32) {
        return Err("bad_scene_widget"); // :184
    }
    // SAFETY: as above; `PETOFF_SCENEWIDGET_SCENE`.
    let scene = unsafe {
        ((widget as *const u8).add(pins::PETOFF_SCENEWIDGET_SCENE.value as usize)
            as *const *mut c_void)
            .read_unaligned()
    };
    if scene.is_null() {
        return Err("no_scene"); // :186
    }
    if !obj_is(scene, pins::PIN_PET, pins::PETV_TOOLSCENE_VT.value as u32) {
        return Err("bad_scene"); // :187
    }
    // SAFETY: as above; `PETOFF_SCENEWIDGET_CAMERA`.
    let camera = unsafe {
        ((widget as *const u8).add(pins::PETOFF_SCENEWIDGET_CAMERA.value as usize)
            as *const *mut c_void)
            .read_unaligned()
    };
    if camera.is_null() {
        return Err("no_camera"); // :189
    }
    if !obj_is(camera, pins::PIN_PET, pins::PETV_SCENECAMERA_VT.value as u32) {
        // :190-197: fail closed and log the observed vtable RVA.
        log::line("pet.render: camera does not match PETV_SCENECAMERA_VT (candidate), observed:");
        if crate::ops::ident::mem_ok(camera) {
            // SAFETY: `mem_ok` just proved the vptr word readable; the C reads it the same way.
            let vptr = unsafe { (camera as *const u64).read_unaligned() };
            let base = crate::modules::live().base(pins::PIN_PET);
            tag_lognum("pet.render", "camera vtable rva ", vptr.wrapping_sub(base));
        }
        return Err("bad_camera");
    }
    Ok((scene, camera))
}

/// No pet.dll on this target: `resolve_pet` answers null, the C's `pet_unavailable`.
#[cfg(not(windows))]
fn pet_chain() -> Result<(*mut c_void, *mut c_void), &'static str> {
    Err("pet_unavailable")
}
