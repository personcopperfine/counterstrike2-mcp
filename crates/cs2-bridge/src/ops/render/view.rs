//! `view.render`: Hammer's own offscreen render to a PNG (`mcp_bridge.c:2500-2857`).
//!
//! The C arm (`mcp_bridge.c:6063-6077`) first resolves the active document (`active_doc`), then
//! `op_view_render` walks document -> session -> view manager -> preferred 3D view, reads the
//! viewport camera, validates the parameters, builds Hammer's default frustum, renders through
//! `CMapViewMgr::RenderViewToMemory` and writes `Z:\app\renders\<stem>.png`.
//!
//! [`view_reply_into`] is the pure result builder; [`live`] is the Windows reader that fills its
//! inputs. The host tests hold the builder to the C's `sb_add` sequence byte for byte, and the
//! parameter contract ([`super::view_params`]) to every refusal.

use crate::entity_json::Sb;
use crate::ops::OpErr;
use crate::queue::Request;

use super::{finish_render, Stats};

/// The C's `static char rbuf[4096]` for both Hammer arms (`mcp_bridge.c:6052`, `:6065`).
const RESULT_BUF: usize = 4096;

/// `active_doc`'s refusal data buffer (`mcp_bridge.c:6067`, `char data[192]`).
const NO_DOC_DATA_CAP: usize = 192;

/// The `view.render` reply's inputs (`mcp_bridge.c:2815-2855`): the written file, the camera that
/// was rendered, the viewport it came from, the view's own flags and the pixel statistics.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct ViewReply {
    pub path: Vec<u8>,
    pub file: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub fov: f32,
    pub aspect: f32,
    pub znear: f32,
    pub zfar: f32,
    /// `"explicit"` / `"mixed"` / `"viewport"` (`:2830`).
    pub source: &'static str,
    pub viewport_origin: [f32; 3],
    pub viewport_angles: [f32; 3],
    pub viewport_fov: f32,
    pub viewport_aspect: f32,
    pub viewport_znear: f32,
    pub viewport_zfar: f32,
    pub view_type: u32,
    pub render_mode: u32,
    pub views: u32,
    /// `"hidden"` or `"shown"` (`:2842`).
    pub tool_materials: &'static str,
    pub format: i64,
    pub srgb: bool,
    pub stats: Stats,
    pub crc: u32,
    pub render_ms: u64,
    pub encode_ms: u64,
}

/// The whole `view.render` result object (`mcp_bridge.c:2819-2855`), byte for byte.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn view_reply_into(out: &mut Sb, r: &ViewReply) {
    out.add(b"{\"path\":"); // :2819
    out.addjson(&r.path);
    out.add(b",\"file\":");
    out.addjson(&r.file);
    out.add(b",\"width\":");
    out.addnum(u64::from(r.width));
    out.add(b",\"height\":");
    out.addnum(u64::from(r.height));
    out.add(b",\"bytes\":");
    out.addnum(r.bytes);
    out.add(b",\"camera\":{\"origin\":"); // :2824
    super::sb_addf3(out, r.origin);
    out.add(b",\"angles\":");
    super::sb_addf3(out, r.angles);
    out.add(b",\"fov\":");
    out.addf(r.fov);
    out.add(b",\"aspect\":"); // :2827
    out.addf(r.aspect);
    out.add(b",\"znear\":");
    out.addf(r.znear);
    out.add(b",\"zfar\":");
    out.addf(r.zfar);
    out.add(b",\"source\":"); // :2830
    out.addjson(r.source.as_bytes());
    out.add(b",\"viewport\":{\"origin\":");
    super::sb_addf3(out, r.viewport_origin);
    out.add(b",\"angles\":");
    super::sb_addf3(out, r.viewport_angles);
    out.add(b",\"fov\":");
    out.addf(r.viewport_fov);
    out.add(b",\"aspect\":");
    out.addf(r.viewport_aspect);
    out.add(b",\"znear\":");
    out.addf(r.viewport_znear);
    out.add(b",\"zfar\":");
    out.addf(r.viewport_zfar);
    out.add(b"}}");
    out.add(b",\"view\":{\"type\":"); // :2838
    out.addnum(u64::from(r.view_type));
    out.add(b",\"renderMode\":");
    out.addnum(u64::from(r.render_mode));
    out.add(b",\"views\":");
    out.addnum(u64::from(r.views));
    out.add(b"}");
    out.add(b",\"toolMaterials\":"); // :2842
    out.addjson(r.tool_materials.as_bytes());
    out.add(b",\"format\":");
    out.addnum(r.format as u64);
    out.add(b",\"srgb\":"); // :2844
    out.add(if r.srgb { b"true" } else { b"false" });
    super::add_stats(out, &r.stats, u64::from(r.width) * u64::from(r.height), r.crc); // :2845-2852
    out.add(b",\"ms\":{\"render\":"); // :2853
    out.addnum(r.render_ms);
    out.add(b",\"encode\":");
    out.addnum(r.encode_ms);
    out.add(b"}}");
}

/// `view.render`'s arm: `active_doc` first (`mcp_bridge.c:6064-6072`), then the op.
pub(super) fn handler(request: &Request) -> String {
    let mut buf = vec![0u8; RESULT_BUF];
    let mut out = Sb::new(&mut buf);
    let outcome = match crate::ops::hammer::game().active_doc() {
        Err(reason) => Err(no_active_document(reason)),
        Ok(doc) => op_view_render(doc, request.params.as_deref(), &mut out),
    };
    finish_render(&request.id, outcome, &out)
}

/// The `-32003 NoActiveDocument` arm (`mcp_bridge.c:6066-6071`): `{"reason":<reason>}` built in
/// the C's 192-byte scratch.
fn no_active_document(reason: &str) -> OpErr {
    let mut buf = vec![0u8; NO_DOC_DATA_CAP];
    let mut db = Sb::new(&mut buf);
    db.add(b"{\"reason\":");
    db.addjson(reason.as_bytes());
    db.add(b"}");
    OpErr {
        code: -32003,
        name: "NoActiveDocument",
        data: String::from_utf8_lossy(db.as_cstr()).into_owned(),
    }
}

/// `op_view_render` (`mcp_bridge.c:2668-2857`).
#[cfg(windows)]
fn op_view_render(
    doc: crate::ops::hammer::Doc,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    live::op_view_render(doc, params, out)
}

/// No hammer.dll on this target: the method gate answers `-32017` before this body, and the
/// Rust-only refusal here names the missing platform if a test drives it directly.
#[cfg(not(windows))]
fn op_view_render(
    _doc: crate::ops::hammer::Doc,
    _params: Option<&str>,
    _out: &mut Sb,
) -> Result<(), OpErr> {
    Err(OpErr::new(
        -32603,
        "InternalError",
        "view.render needs the Windows target: this build has no hammer.dll half",
    ))
}

/// The Windows reader.
#[cfg(windows)]
mod live {
    use core::ffi::c_void;
    use core::mem::{offset_of, MaybeUninit};
    use core::ptr::addr_of;

    use s2sdk_sys::generated::{
        CMapDocSession, CMapView, CMapViewMgr, CFrustum, HammerEditorSettings_t,
        IToolRenderableViewHost, ToolCameraUnk_t,
    };

    use crate::entity_json::Sb;
    use crate::generated::pins;
    use crate::log;
    use crate::modules;
    use crate::ops::hammer::{self, Doc};
    use crate::ops::ident::obj_is;
    use crate::ops::OpErr;

    use super::super::{
        convert_view, crc32, ensure_renders_dir, fill_sentinel_strided, frustum_set, now_ms,
        png_encode, renders_dir, view_shape, view_viewport, write_failed_reason, write_file_atomic,
        CameraFacts,
    };
    use super::{view_reply_into, ViewReply};

    /// `g_vrFrustum` (`mcp_bridge.c:2516`): the C's 16-aligned process static, **not** a stack
    /// local - it is zeroed at load and each call only overwrites the fields `FrustumInit` +
    /// `frustum_set` + `FrustumUpdate` write, so a render sees the previous call's leftovers
    /// exactly as the C does.
    ///
    /// Main thread only (the tick's drain), like the C: no lock because no other thread may
    /// reach this scratch block.
    #[repr(C, align(16))]
    struct FrustumBlock {
        frustum: CFrustum,
    }

    static mut VR_FRUSTUM: MaybeUninit<FrustumBlock> = MaybeUninit::zeroed();

    // `S2_SIZEOF_CFrustum == 0x420` is a multiple of 16, so the wrapper adds no padding.
    const _: () = assert!(core::mem::size_of::<FrustumBlock>() == core::mem::size_of::<CFrustum>());
    const _: () = assert!(core::mem::align_of::<FrustumBlock>() == 16);

    pub(super) fn op_view_render(
        doc: Doc,
        params: Option<&str>,
        out: &mut Sb,
    ) -> Result<(), OpErr> {
        // :2669: u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
        let hb = modules::live().base(pins::PIN_HAMMER) as usize;
        // :2670-2671: CMapDocSession_Owner(CMapDoc_Session(doc)) -> its m_pViewMgr.
        // SAFETY: `doc` passed `obj_is(HV_DOC_VT)` in `active_doc`, and a CMapDoc is embedded in
        // its CMapDocSession at `m_Doc`; the owner was identity-checked as `HV_SESSION_VT` on
        // that same walk.
        let session = unsafe {
            let doc_session = (doc.0 as *const u8).sub(offset_of!(CMapDocSession, m_Doc));
            (*(doc_session as *const CMapDocSession)).m_pOwner
        };
        let mgr = if session.is_null() {
            0
        } else {
            // SAFETY: `session` is the identity-checked CHammerEditorSession.
            (unsafe { (*session).m_pViewMgr }) as usize
        };
        if mgr == 0
            || !obj_is(
                mgr as *const c_void,
                pins::PIN_HAMMER,
                pins::HV_MAPVIEWMGR_VT.value as u32,
            )
        {
            return Err(OpErr::new(
                -32019,
                "NoViewport",
                if mgr != 0 {
                    "bad_view_manager"
                } else {
                    "no_view_manager"
                }, // :2672-2674
            ));
        }
        let hfn =
            crate::ops::hammer::bind::hfn().expect("resolve_hammer bound g_hfn before active_doc succeeded"); // :2676
        // SAFETY: `mgr` passed `obj_is(HV_MAPVIEWMGR_VT)`; GetPreferred3DView is hammer.dll's
        // pinned `HFN_GET_PREFERRED_3D_VIEW`.
        let view = unsafe { (hfn.get_preferred_3d_view)(mgr as *mut c_void) };
        if view.is_null() {
            return Err(OpErr::new(-32019, "NoViewport", "no_3d_view")); // :2677
        }
        if !obj_is(view, pins::PIN_HAMMER, pins::HV_MAPVIEW_VT.value as u32) {
            return Err(OpErr::new(-32019, "NoViewport", "bad_view")); // :2678
        }

        // :2680-2688: the size/format block is checked BEFORE the camera is touched, as in the
        // C (a bad width never reads game memory).
        let shape = view_shape(params)?;

        // :2691-2695: the live 3D camera (the camera object embeds a CFrustum at +0x50).
        // SAFETY: `view` passed `obj_is(HV_MAPVIEW_VT)` and slot 6 of that vtable is pinned
        // (`CMapView::GetCamera`), so the typed field is the game's own getter.
        let get_camera = unsafe {
            IToolRenderableViewHost::vtable(
                view as *const CMapView as *const IToolRenderableViewHost,
            )
        }
        .GetCamera
        .expect("pinned CMapView::GetCamera slot");
        // SAFETY: the verified getter, called with the identity-checked view.
        let cam = unsafe { get_camera(view as *mut CMapView as *mut IToolRenderableViewHost) };
        let mut v_origin = [0.0f32; 3];
        let mut v_angles = [0.0f32; 3];
        // SAFETY: the camera is the view's own member; the getters are hammer.dll's pinned
        // `HFN_CAMERA_GET_POSITION` / `HFN_CAMERA_GET_ANGLES` and each writes three floats.
        unsafe {
            (hfn.camera_get_position)(cam.cast(), v_origin.as_mut_ptr());
            (hfn.camera_get_angles)(cam.cast(), v_angles.as_mut_ptr());
        }
        // SAFETY: `cam` came from the verified getter; the frustum is the embedded member at the
        // generated offset (the C's `CCamera_Frustum`, `s2sdk.h:533`). Every field is read
        // unaligned through a raw pointer - no reference into game memory is formed.
        let fr = unsafe {
            (cam as *const u8).add(offset_of!(ToolCameraUnk_t, m_Frustum)) as *const CFrustum
        };
        let live = CameraFacts {
            origin: v_origin,
            angles: v_angles,
            // SAFETY: as above; reached through the identity-checked camera.
            fov: unsafe { addr_of!((*fr).m_camera.m_flFOVX).read_unaligned() },
            aspect: unsafe { addr_of!((*fr).m_camera.m_flAspect).read_unaligned() },
            znear: unsafe { addr_of!((*fr).m_camera.m_flZNear).read_unaligned() },
            zfar: unsafe { addr_of!((*fr).m_camera.m_flZFar).read_unaligned() },
        };

        let vp = view_viewport(params, &live, shape)?;

        // :2733-2737: Hammer's default template, then the screenshot dialog's fields, in the C's
        // process-wide `g_vrFrustum` (not a stack local): `FrustumInit` re-initialises it on top
        // of the previous call's bytes, which is what the C's static does.
        // SAFETY: `VR_FRUSTUM` is the process static the C uses, 16-aligned and zeroed at load;
        // the main thread is the only writer (the tick's drain). The two calls are hammer.dll's
        // pinned `HFN_FRUSTUM_INIT` / `HFN_FRUSTUM_UPDATE`, and `frustum_set` writes the fields
        // the two calls expect to find.
        let fr = unsafe {
            let p = core::ptr::addr_of_mut!(VR_FRUSTUM).cast::<FrustumBlock>();
            let f = core::ptr::addr_of_mut!((*p).frustum);
            (hfn.frustum_init)(f.cast());
            frustum_set(
                &mut *f,
                vp.origin,
                vp.angles,
                vp.fov,
                vp.aspect,
                vp.znear,
                vp.zfar,
            );
            (hfn.frustum_update)(f.cast());
            f
        };

        // :2739-2752: the sentinel buffers.
        let bpp = if vp.format == 0 { 4u32 } else { 3u32 };
        let npx = u64::from(vp.w) * u64::from(vp.h);
        let mut px = vec![0u8; (npx * u64::from(bpp)) as usize];
        fill_sentinel_strided(&mut px);

        // :2754-2760: hideToolMaterials flips the viewport's "Show Tools Materials" toggle.
        let app = hammer::game().app().unwrap_or(0);
        // `S2HammerEditorSettings *settings = (S2HammerEditorSettings *)(hb + HG_EDITOR_SETTINGS)`
        // (`:2754`): one pointer, read before the render (`:2755`) and again in the reply
        // (`:2842`), as the C does.
        // SAFETY: `HG_EDITOR_SETTINGS` is a pinned global of the verified module; the pointer is
        // only formed here, and every read through it is a u8, never a Rust bool.
        let settings =
            (hb + pins::HG_EDITOR_SETTINGS.value as usize) as *const HammerEditorSettings_t;
        let show_tools = unsafe {
            addr_of!((*settings).m_bShowToolsMaterials)
                .cast::<u8>()
                .read_unaligned()
        } != 0; // :2755
        let mut tools_toggled = false;
        if vp.hide_tools && app != 0 && show_tools {
            // SAFETY: `app` is the identity-checked CHammerApp; the toggle is hammer.dll's
            // pinned `HFN_TOGGLE_TOOLS_MATERIALS` handler.
            unsafe { (hfn.toggle_tools_materials)(app as *mut c_void) };
            tools_toggled = true; // :2757-2759
        }
        let t0 = now_ms();
        // SAFETY: `session` and `mgr`/`view` all passed their identity checks; the calls are
        // hammer.dll's pinned `HFN_SESSION_UPDATE_VIEWS` and `HFN_RENDER_VIEW_TO_MEMORY`.
        unsafe {
            (hfn.session_update_views)(session.cast()); // :2762
            (hfn.render_view_to_memory)(
                mgr as *mut c_void,
                fr.cast(),
                vp.w as i32,
                vp.h as i32,
                vp.format as i32,
                px.as_mut_ptr().cast(),
                view, // :2763
            );
        }
        let t1 = now_ms();
        if tools_toggled {
            // SAFETY: as above; the toggle is restored before anything can fail.
            unsafe { (hfn.toggle_tools_materials)(app as *mut c_void) };
        } // :2765

        // :2767-2797: convert + stats.
        let (raw, stats) = convert_view(&px, vp.w, vp.h, bpp, vp.srgb);
        drop(px);
        if stats.untouched == npx {
            return Err(OpErr::new(
                -32020,
                "RenderFailed",
                "readback did not run (no pixels written)", // :2795
            ));
        }
        // :2798-2803
        let crc = crc32(&raw);
        let png = png_encode(&raw, vp.w, vp.h)
            .ok_or_else(|| OpErr::new(-32020, "RenderFailed", "png encode: out of memory"))?;
        drop(raw);

        // :2805-2813: the path, the atomic write, the log lines.
        let dir = renders_dir();
        ensure_renders_dir(&dir);
        // the C's SB was 200 bytes; a native dir is longer. 290 + `.tmp` still fits
        // write_file_atomic's 300-byte temp name
        let mut pathbuf = vec![0u8; 290];
        let mut ps = Sb::new(&mut pathbuf);
        ps.add(&dir);
        ps.add(&vp.stem);
        ps.add(b".png");
        let wrote = !ps.ovf() && write_file_atomic(ps.as_cstr(), &png);
        let path = ps.as_cstr().to_vec();
        let t2 = now_ms();
        if !wrote {
            return Err(OpErr::new(
                -32020,
                "RenderFailed",
                &write_failed_reason(&dir), // :2811
            ));
        }
        log::line("view.render: wrote"); // :2812
        log::line(&String::from_utf8_lossy(&path)); // :2813

        // :2815-2818: the file name in the C's 96-byte SB.
        let mut fbuf = vec![0u8; 96];
        let mut fs = Sb::new(&mut fbuf);
        fs.add(&vp.stem);
        fs.add(b".png");
        let file = fs.as_cstr().to_vec();

        // :2838-2840: the view's own flags and the manager's view count.
        // SAFETY: `view` passed `obj_is(HV_MAPVIEW_VT)`; the two members are the generated
        // CMapView fields the C reads.
        let (view_type, render_mode) = unsafe {
            let v = view as *const CMapView;
            (
                addr_of!((*v).m_nViewType).read_unaligned() as u32,
                addr_of!((*v).m_nRenderMode).read_unaligned() as u32,
            )
        };
        // SAFETY: `mgr` passed `obj_is(HV_MAPVIEWMGR_VT)`; `m_Views` is the generated
        // CUtlVector's bytes and its first word is the C's `m_Size` (`CMapViewMgr_ViewCount`,
        // `s2sdk.h:530`).
        let views = unsafe {
            let m = mgr as *const u8;
            (m.add(offset_of!(CMapViewMgr, m_Views)) as *const i32).read_unaligned() as u32
        };

        // :2827-2836: the C re-reads the live frustum's fields while it builds the reply, after
        // `RenderViewToMemory` and the tools-materials restore - it does not print the values the
        // render started from. Re-read them here, from the same camera the render used.
        // SAFETY: `fr` is the frustum embedded in the camera `GetCamera` returned for the
        // identity-checked view (the C's `live`); every field is read unaligned through the raw
        // pointer, exactly as at `:2692-2694`.
        let (vp_fov, vp_aspect, vp_znear, vp_zfar) = unsafe {
            (
                addr_of!((*fr).m_camera.m_flFOVX).read_unaligned(),
                addr_of!((*fr).m_camera.m_flAspect).read_unaligned(),
                addr_of!((*fr).m_camera.m_flZNear).read_unaligned(),
                addr_of!((*fr).m_camera.m_flZFar).read_unaligned(),
            )
        };
        // :2842: `settings->showToolsMaterials`, re-read after the restore (the C's `sb_add`
        // argument), again as a u8.
        // SAFETY: `settings` is the pinned global of the verified hammer module.
        let show_tools_reply = unsafe {
            addr_of!((*settings).m_bShowToolsMaterials)
                .cast::<u8>()
                .read_unaligned()
        } != 0;

        let reply = ViewReply {
            path,
            file,
            width: vp.w,
            height: vp.h,
            bytes: png.len() as u64,
            origin: vp.origin,
            angles: vp.angles,
            fov: vp.fov,
            aspect: vp.aspect,
            znear: vp.znear,
            zfar: vp.zfar,
            source: vp.source,
            viewport_origin: live.origin,
            viewport_angles: live.angles,
            viewport_fov: vp_fov,       // :2829
            viewport_aspect: vp_aspect, // :2831
            viewport_znear: vp_znear,
            viewport_zfar: vp_zfar,
            view_type,
            render_mode,
            views,
            tool_materials: if tools_toggled || !show_tools_reply {
                "hidden"
            } else {
                "shown"
            }, // :2842
            format: vp.format,
            srgb: vp.srgb,
            stats,
            crc,
            render_ms: t1 - t0, // :2853
            encode_ms: t2 - t1,
        };
        view_reply_into(out, &reply);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{view_reply_into, ViewReply};
    use crate::entity_json::Sb;
    use crate::ops::render::Stats;

    /// The whole result object, byte for byte from the C's `sb_add` sequence
    /// (`mcp_bridge.c:2819-2855`), including `sb_addf3`'s triple and `sb_addf`'s three decimals.
    #[test]
    fn view_reply_is_the_c_object() {
        let reply = ViewReply {
            path: b"Z:\\app\\renders\\a.png".to_vec(),
            file: b"a.png".to_vec(),
            width: 64,
            height: 48,
            bytes: 123,
            origin: [1.0, 2.0, 3.0],
            angles: [4.0, 5.0, 6.0],
            fov: 90.0,
            aspect: 1.333,
            znear: 1.0,
            zfar: 9000.0,
            source: "explicit",
            viewport_origin: [0.0, 0.0, 0.0],
            viewport_angles: [0.0, 0.0, 0.0],
            viewport_fov: 90.0,
            viewport_aspect: 1.5,
            viewport_znear: 1.0,
            viewport_zfar: 9000.0,
            view_type: 3,
            render_mode: 2,
            views: 4,
            tool_materials: "shown",
            format: 0,
            srgb: true,
            stats: Stats {
                untouched: 0,
                luma_sum: 38_400, // 12.5 over the 64x48 reply pixels
                non_black: 2,
                luma_min: 1,
                luma_max: 24,
            },
            crc: 0xDEAD_BEEF,
            render_ms: 5,
            encode_ms: 7,
        };
        let mut buf = vec![0u8; 1024];
        let mut out = Sb::new(&mut buf);
        view_reply_into(&mut out, &reply);
        assert!(!out.ovf());
        assert_eq!(
            std::str::from_utf8(out.as_cstr()).unwrap(),
            r#"{"path":"Z:\\app\\renders\\a.png","file":"a.png","width":64,"height":48,"bytes":123,"camera":{"origin":[1,2,3],"angles":[4,5,6],"fov":90,"aspect":1.333,"znear":1,"zfar":9000,"source":"explicit","viewport":{"origin":[0,0,0],"angles":[0,0,0],"fov":90,"aspect":1.500,"znear":1,"zfar":9000}},"view":{"type":3,"renderMode":2,"views":4},"toolMaterials":"shown","format":0,"srgb":true,"stats":{"untouchedPixels":0,"nonBlackPixels":2,"meanLuma":12.500,"minLuma":1,"maxLuma":24,"crc32":"deadbeef"},"ms":{"render":5,"encode":7}}"#
        );
    }

    /// `format:2` reports the C's `(u64)lfmt` and `"srgb":false`, and `toolMaterials` follows the
    /// toggle (`mcp_bridge.c:2842-2844`).
    #[test]
    fn view_reply_reports_rgb888_and_hidden_tools() {
        let reply = ViewReply {
            path: b"p".to_vec(),
            file: b"f".to_vec(),
            width: 16,
            height: 16,
            bytes: 0,
            origin: [0.0; 3],
            angles: [0.0; 3],
            fov: 90.0,
            aspect: 1.0,
            znear: 1.0,
            zfar: 2.0,
            source: "viewport",
            viewport_origin: [0.0; 3],
            viewport_angles: [0.0; 3],
            viewport_fov: 90.0,
            viewport_aspect: 1.0,
            viewport_znear: 1.0,
            viewport_zfar: 2.0,
            view_type: 0,
            render_mode: 0,
            views: 0,
            tool_materials: "hidden",
            format: 2,
            srgb: false,
            stats: Stats::default(),
            crc: 0,
            render_ms: 0,
            encode_ms: 0,
        };
        let mut buf = vec![0u8; 1024];
        let mut out = Sb::new(&mut buf);
        view_reply_into(&mut out, &reply);
        let text = std::str::from_utf8(out.as_cstr()).unwrap();
        assert!(text.contains(r#""toolMaterials":"hidden""#));
        assert!(text.contains(r#""format":2,"srgb":false"#));
        assert!(text.contains(r#""source":"viewport""#));
        assert!(text.contains(r#""crc32":"00000000""#));
    }
}
