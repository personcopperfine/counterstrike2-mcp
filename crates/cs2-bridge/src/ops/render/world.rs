//! `world.render`: the Hammer-less offscreen render of the running map
//! (`mcp_bridge.c:2861-3105`).
//!
//! The engine's own pieces: `SceneSystem_002` Begin/AddView/Finish/Wait, the `Tools` (-> CSGO)
//! rendering pipeline, the view's own scratch colour/depth targets at the requested size and
//! `RenderDevice003::ReadTexturePixels`. The engine objects come from the `Connect` factory by
//! interface name; every object's vtable identity and pinned slots are checked before it is used
//! (`engine_ifaces`, `:2892-2916`). The whole Begin..Wait bracket runs inside this one main-thread
//! call: the engine must not start its own frame in between.
//!
//! [`world_reply_into`] is the pure result builder and [`super::world_params`] the parameter
//! contract; [`op_world_render`] is the engine walk. The engine modules exist on both targets
//! (the generated `scenesystem`/`engine`/`rendersystem` groups are not Windows-only), but the
//! method gate only lets this body run where they verified: without a `Connect` factory the walk
//! fails with the C's own `engine interface not found (Connect factory)`.

#[cfg(windows)]
use core::ffi::{c_char, c_int, c_void};
#[cfg(windows)]
use std::sync::{Mutex, MutexGuard};

#[cfg(windows)]
use s2sdk_sys::generated::{CFrustum, CRenderAttributes};
#[cfg(windows)]
use s2sdk_sys::generated::{
    CUtlStringToken, IRenderDevice, IRenderDevice_slots, IRenderingPipeline, IRenderService,
    ISceneSystem, ISceneSystem_slots, ISceneUtils, ISceneView, RenderViewport_t,
};
#[cfg(windows)]
use s2sdk_sys::generated::{
    IMAGE_FORMAT_BGRA8888, IMAGE_FORMAT_D24S8, IMAGE_FORMAT_D32FS8, IMAGE_FORMAT_RGBA8888,
    RENDER_DEVICE_INTERFACE_VERSION, RENDERSERVICE_INTERFACE_VERSION, SCENESYSTEM_INTERFACE_VERSION,
    SCENEUTILS_INTERFACE_VERSION, TOKEN_OUTPUT_COLOR, TOKEN_OUTPUT_DEPTH, TOKEN_TOOLS,
};

use crate::entity_json::Sb;
#[cfg(windows)]
use crate::generated::pins;
#[cfg(windows)]
use crate::log;
#[cfg(windows)]
use crate::modules;
#[cfg(windows)]
use crate::ops::ident::{obj_is, vt_slots_ok};
use crate::ops::OpErr;
use crate::queue::Request;

use super::{add_stats, finish_render, Stats};
#[cfg(windows)]
use super::{
    convert_word_pixels, crc32, ensure_renders_dir, fill_sentinel_u32, frustum_set, now_ms,
    png_encode, renders_dir, world_params, write_failed_reason, write_file_atomic,
};

/// The C's `static char rbuf[4096]` (`mcp_bridge.c:6052`).
const RESULT_BUF: usize = 4096;

/// `WR_COLOR_FMT` (`mcp_bridge.c:2870`): the colour scratch format.
#[cfg(windows)]
const WR_COLOR_FMT: i32 = IMAGE_FORMAT_BGRA8888 as i32;

/// The C's `S2RenderTargetDesc` (`s2sdk.h:85`): `{int w, h; u8 msaa, pad;}` - a
/// bridge-owned layout with no upstream type.
#[repr(C)]
#[allow(dead_code)] // constructed and passed by pointer; the C never reads the fields either
#[cfg(windows)]
struct RenderTargetDesc {
    w: i32,
    h: i32,
    msaa: u8,
    pad: u8,
}

/// The C's `S2Rect` (`s2sdk.h:88`), the same layout as upstream `Rect_t`.
#[repr(C)]
#[allow(dead_code)] // constructed and passed by pointer
#[cfg(windows)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// `g_wrAttrs` (`mcp_bridge.c:2871`): a 1024-byte, 16-aligned block whose first 928 bytes are
/// the generated `CRenderAttributes` union member.
#[repr(C, align(16))]
#[cfg(windows)]
struct AttrsBlock {
    attrs: CRenderAttributes,
    _slack: [u8; 1024 - core::mem::size_of::<CRenderAttributes>()],
}

#[cfg(windows)]
const _: () = assert!(core::mem::size_of::<CRenderAttributes>() == 928);
#[cfg(windows)]
const _: () = assert!(core::mem::size_of::<AttrsBlock>() == 1024);

/// `g_wrAttrs` / `g_wrFrustum` (`mcp_bridge.c:2871-2872`): process-wide, main thread only, as in
/// the C.
#[cfg(windows)]
static mut WR_ATTRS: core::mem::MaybeUninit<AttrsBlock> = core::mem::MaybeUninit::uninit();
#[cfg(windows)]
static mut WR_FRUSTUM: core::mem::MaybeUninit<CFrustum> = core::mem::MaybeUninit::uninit();

/// The C's `EngineFns` (`s2sdk.h:749-753`): non-virtual, non-exported engine2 functions.
#[derive(Clone, Copy)]
#[cfg(windows)]
struct EngineFns {
    attrs_ctor: unsafe extern "C" fn(*mut CRenderAttributes),
    attrs_dtor: unsafe extern "C" fn(*mut CRenderAttributes),
    attrs_set_handle: unsafe extern "C" fn(*mut c_void, u32, i64),
}

/// The C's `SceneFns` (`s2sdk.h:745-748`).
#[derive(Clone, Copy)]
#[cfg(windows)]
struct SceneFns {
    frustum_init: unsafe extern "C" fn(*mut CFrustum),
    frustum_update: unsafe extern "C" fn(*mut CFrustum),
}

/// The C's `EngineIfaces` + the bound function tables (`mcp_bridge.c:2877-2882`,
/// `:2905-2914`). Raw pointers are kept as `usize` so the `Mutex` stays `Send`.
#[derive(Clone, Copy)]
#[cfg(windows)]
struct Engine {
    scene_system: usize,
    scene_utils: usize,
    render_service: usize,
    render_device: usize,
    scene: SceneFns,
    engine: EngineFns,
}

/// C `g_eng` + `g_engOk` (`mcp_bridge.c:2881-2882`): resolved once, on the main thread.
#[cfg(windows)]
static ENG: Mutex<Option<Engine>> = Mutex::new(None);

#[cfg(windows)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `factory_get` (`mcp_bridge.c:2884-2890`): one interface by name from the `Connect` factory.
#[cfg(windows)]
fn factory_get(name: &str) -> *mut c_void {
    let factory = crate::toolsys::connect_factory();
    if factory.is_null() {
        return core::ptr::null_mut(); // :2885
    }
    let Ok(cname) = std::ffi::CString::new(name) else {
        return core::ptr::null_mut();
    };
    // `S2CreateInterfaceFn` (`s2sdk.h:60`): `void *(const char *, int *)`.
    type CreateInterfaceFn = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
    // SAFETY: `g_connectFactory` is the factory `Connect` was handed (a real app-system factory);
    // the signature is the C header's, `cname` outlives the call and `ret` is its out-parameter.
    let get: CreateInterfaceFn = unsafe { core::mem::transmute(factory) };
    let mut ret: c_int = -1;
    // SAFETY: as above.
    let p = unsafe { get(cname.as_ptr(), &mut ret) };
    if p.is_null() {
        log::line("iface: not found: "); // :2888
        log::line(name);
    }
    p // :2889
}

/// `engine_ifaces` (`mcp_bridge.c:2892-2916`): resolve and bind the engine objects once.
#[cfg(windows)]
fn engine_ifaces() -> Result<Engine, &'static str> {
    if let Some(engine) = *lock(&ENG) {
        return Ok(engine); // :2893
    }
    let mut why = "engine interface identity check failed"; // :2894
    let ss = factory_get(SCENESYSTEM_INTERFACE_VERSION); // :2895
    let su = factory_get(SCENEUTILS_INTERFACE_VERSION); // :2896
    let rs = factory_get(RENDERSERVICE_INTERFACE_VERSION); // :2897
    let rd = factory_get(RENDER_DEVICE_INTERFACE_VERSION); // :2898
    if ss.is_null() || su.is_null() || rs.is_null() || rd.is_null() {
        why = "engine interface not found (Connect factory)"; // :2899
        return Err(why);
    }
    if !obj_is(ss, pins::PIN_SCENESYSTEM, pins::SV_SCENESYSTEM_VT.value as u32)
        || !obj_is(su, pins::PIN_SCENESYSTEM, pins::SV_SCENEUTILS_VT.value as u32)
        || !obj_is(rs, pins::PIN_ENGINE2, pins::EV_RENDERSERVICE_VT.value as u32)
        || !obj_is(rd, pins::PIN_RSV, pins::RV_RENDERDEVICE_VT.value as u32)
    {
        return Err(why); // :2900-2902
    }
    // :2904: the per-call objects' vtables' slots now.
    if !vt_slots_ok(pins::PIN_SCENESYSTEM, pins::SV_SCENEVIEW_VT.value as u32)
        || !vt_slots_ok(pins::PIN_SCENESYSTEM, pins::SV_PIPELINE_ALIAS_VT.value as u32)
    {
        return Err(why);
    }
    // :2906-2912: bind the five non-virtual functions from their pinned RVAs.
    let sbase = modules::live().base(pins::PIN_SCENESYSTEM) as usize;
    let ebase = modules::live().base(pins::PIN_ENGINE2) as usize;
    macro_rules! bind {
        ($base:expr, $pin:ident, $ty:ty) => {{
            let address = $base + pins::$pin.value as usize;
            // SAFETY: `address` is a verified module's image at the pinned function RVA and
            // `$ty` is the C header's own signature (`s2sdk.h:745-753`).
            unsafe { core::mem::transmute::<usize, $ty>(address) }
        }};
    }
    let engine = Engine {
        scene_system: ss as usize,
        scene_utils: su as usize,
        render_service: rs as usize,
        render_device: rd as usize,
        scene: SceneFns {
            frustum_init: bind!(sbase, SFN_FRUSTUM_INIT, unsafe extern "C" fn(*mut CFrustum)),
            frustum_update: bind!(sbase, SFN_FRUSTUM_UPDATE, unsafe extern "C" fn(*mut CFrustum)),
        },
        engine: EngineFns {
            attrs_ctor: bind!(
                ebase,
                EFN_ATTRS_CTOR,
                unsafe extern "C" fn(*mut CRenderAttributes)
            ),
            attrs_dtor: bind!(
                ebase,
                EFN_ATTRS_DTOR,
                unsafe extern "C" fn(*mut CRenderAttributes)
            ),
            attrs_set_handle: bind!(
                ebase,
                EFN_ATTRS_SET_HANDLE,
                unsafe extern "C" fn(*mut c_void, u32, i64)
            ),
        },
    };
    *lock(&ENG) = Some(engine); // :2913
    log::line("iface: engine interfaces resolved by name"); // :2914
    Ok(engine)
}

/// `void *(*WaitForRenderingToComplete)(ISceneSystem *)` (`s2sdk.h:160`): the C's `u8`
/// return, never a Rust `bool`.
#[cfg(windows)]
type FnWait = unsafe extern "C" fn(*mut ISceneSystem) -> u8;
/// `u8 (*IsUsingReverseDepth)(IRenderDevice *)` (`s2sdk.h:274`).
#[cfg(windows)]
type FnReverseDepth = unsafe extern "C" fn(*mut IRenderDevice) -> u8;
/// `u8 (*ReadTexturePixels)(IRenderDevice *, void *texture, S2Rect *src, u32 a3, int a4,
/// S2Rect *dst, void *pixels, int format, int pitch)` (`s2sdk.h:271-273`).
#[cfg(windows)]
type FnReadPixels = unsafe extern "C" fn(
    *mut IRenderDevice,
    *mut c_void,
    *mut Rect,
    u32,
    i32,
    *mut Rect,
    *mut c_void,
    i32,
    i32,
) -> u8;

/// One vtable entry by slot, for the three `bool`-returning entries the generated fields would
/// hand back as a Rust `bool` (the wave-2 rule: read the C's `u8` instead).
///
/// # Safety
///
/// `obj` must point at an object whose vptr is the pinned class vtable, and `slot` one of that
/// class's generated slot constants for a function of type `T`.
#[cfg(windows)]
unsafe fn slot_fn<T: Copy>(obj: *const c_void, slot: usize) -> T {
    // SAFETY: the vptr lies at offset 0 of every polymorphic object (the C reads it the same
    // way), and the caller's identity check pinned it to the class's vtable.
    let vptr = unsafe { obj.cast::<*const *const c_void>().read_unaligned() };
    let entries = vptr.cast::<*const c_void>();
    // SAFETY: `slot` is a generated slot constant of that class, inside its VTLEN.
    let entry = unsafe { *entries.add(slot) };
    // SAFETY: `T` is the signature of the vtable entry at `slot`; the bit pattern is a function
    // pointer of that type.
    unsafe { core::mem::transmute_copy(&entry) }
}

/// The `BeginRenderingViews` … `FinishRenderingViews` bracket (`mcp_bridge.c:2990-3015`).
///
/// The C cannot panic between the two calls, but Rust can: every vtable entry is an `Option`
/// whose `.expect()` is only unreachable *after* the module gate's slot checks, and the
/// scene-view table cannot even be read before `AddView` returned its object. A drop without
/// `open = false` therefore runs the C's own finish side, so a panic still answers the tick's
/// `-32603` and never leaves an engine frame open.
#[cfg(windows)]
struct Frame {
    ss: *mut ISceneSystem,
    finish: unsafe extern "C" fn(*mut ISceneSystem, f32),
    wait: FnWait,
    open: bool,
}

#[cfg(windows)]
impl Drop for Frame {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        // SAFETY: `open` is only set right after `BeginRenderingViews` opened this frame on `ss`;
        // the two entries are the pinned slots 25/26 of the identity-checked scene system, and
        // `FLT_MAX` is the C's "use the platform time".
        unsafe {
            (self.finish)(self.ss, f32::MAX);
            (self.wait)(self.ss);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The result
// ---------------------------------------------------------------------------------------------

/// The `world.render` reply's inputs (`mcp_bridge.c:3081-3103`).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct WorldReply {
    pub path: Vec<u8>,
    pub file: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub fov: f32,
    pub znear: f32,
    pub zfar: f32,
    pub srgb: bool,
    pub stats: Stats,
    pub crc: u32,
    pub render_ms: u64,
    pub readback_ms: u64,
    pub encode_ms: u64,
}

/// The whole `world.render` result object (`mcp_bridge.c:3081-3104`), byte for byte.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn world_reply_into(out: &mut Sb, r: &WorldReply) {
    out.add(b"{\"path\":"); // :3081
    out.addjson(&r.path);
    out.add(b",\"file\":");
    out.addjson(&r.file);
    out.add(b",\"width\":");
    out.addnum(u64::from(r.width));
    out.add(b",\"height\":");
    out.addnum(u64::from(r.height));
    out.add(b",\"bytes\":");
    out.addnum(r.bytes);
    out.add(b",\"camera\":{\"origin\":"); // :3086
    super::sb_addf3(out, r.origin);
    out.add(b",\"angles\":");
    super::sb_addf3(out, r.angles);
    out.add(b",\"fov\":");
    out.addf(r.fov);
    out.add(b",\"znear\":");
    out.addf(r.znear);
    out.add(b",\"zfar\":");
    out.addf(r.zfar);
    out.add(b"},\"pipeline\":\"Tools->CSGO\",\"srgb\":"); // :3091
    out.add(if r.srgb { b"true" } else { b"false" });
    add_stats(out, &r.stats, u64::from(r.width) * u64::from(r.height), r.crc); // :3092-3099
    out.add(b",\"ms\":{\"render\":"); // :3100
    out.addnum(r.render_ms);
    out.add(b",\"readback\":");
    out.addnum(r.readback_ms);
    out.add(b",\"encode\":");
    out.addnum(r.encode_ms);
    out.add(b"}}");
}

/// `world.render`'s arm (`mcp_bridge.c:6051-6061`).
pub(super) fn handler(request: &Request) -> String {
    let mut buf = vec![0u8; RESULT_BUF];
    let mut out = Sb::new(&mut buf);
    let outcome = op_world_render(request.params.as_deref(), &mut out);
    finish_render(&request.id, outcome, &out)
}

// ---------------------------------------------------------------------------------------------
// `op_world_render` (`mcp_bridge.c:2918-3105`)
// ---------------------------------------------------------------------------------------------

/// `engine interface not found (Connect factory)` (`mcp_bridge.c:2899`): the refusal
/// `engine_ifaces` gives a host with no `Connect` factory, which is every non-Windows target.
#[cfg(not(windows))]
const NO_FACTORY: &str = "engine interface not found (Connect factory)";

/// `op_world_render` (`mcp_bridge.c:2918-3105`).
#[cfg(windows)]
fn op_world_render(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let eng = engine_ifaces().map_err(|why| OpErr::new(-32020, "RenderFailed", why))?; // :2919-2920
    render_with(eng, params, out)
}

/// No `Connect` factory on this target (`toolsys::connect_factory()` is only set by the Windows
/// tools framework's `Connect`), so `engine_ifaces` cannot succeed: this is the arm the C takes.
#[cfg(not(windows))]
fn op_world_render(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let _ = (params, out);
    Err(OpErr::new(-32020, "RenderFailed", NO_FACTORY))
}

/// The engine walk after the interfaces resolved (Windows only: the generated per-target vtable
/// shapes differ, and only this target can resolve them).
#[cfg(windows)]
fn render_with(eng: Engine, params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let ss = eng.scene_system as *mut ISceneSystem;
    let rd = eng.render_device as *mut IRenderDevice;
    let rs = eng.render_service as *mut IRenderService;
    let su = eng.scene_utils as *mut ISceneUtils;

    // :2923: world = g_eng.renderService->vt->GetSceneWorld(renderService)
    // SAFETY: `rs` passed `obj_is(EV_RENDERSERVICE_VT)`; GetSceneWorld is that vtable's pinned
    // slot 29.
    let world = unsafe {
        IRenderService::vtable(rs)
            .GetSceneWorld
            .expect("pinned IRenderService::GetSceneWorld slot")(rs)
    };
    if world.is_null() {
        return Err(OpErr::new(
            -32021,
            "NoGameWorld",
            "no map is loaded in the engine (map <name>)", // :2924
        ));
    }
    if !obj_is(world, pins::PIN_SCENESYSTEM, pins::SV_SCENEWORLD_VT.value as u32) {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            "scene world identity check failed", // :2925
        ));
    }
    // :2926: pipe = sceneUtils->vt->FindRenderingPipeline(sceneUtils, S2_CONST_TOKEN_TOOLS)
    // SAFETY: `su` passed `obj_is(SV_SCENEUTILS_VT)`; FindRenderingPipeline is its pinned slot.
    let pipe = unsafe {
        ISceneUtils::vtable(su)
            .FindRenderingPipeline
            .expect("pinned ISceneUtils::FindRenderingPipeline slot")(
            su,
            CUtlStringToken {
                m_nHashCode: TOKEN_TOOLS,
            },
        )
    };
    if !obj_is(pipe.cast(), pins::PIN_SCENESYSTEM, pins::SV_PIPELINE_ALIAS_VT.value as u32) {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            "Tools rendering pipeline not found", // :2927
        ));
    }

    let wp = world_params(params)?; // :2929-2948
    let npx = u64::from(wp.w) * u64::from(wp.h);
    let mut px = vec![0u8; (npx * 4) as usize];
    fill_sentinel_u32(&mut px); // :2959

    // :2962-2965: the camera.
    // SAFETY: `WR_FRUSTUM` is the process-wide aligned scratch block the C uses, and the two
    // calls are scenesystem.dll's pinned `SFN_FRUSTUM_INIT` / `SFN_FRUSTUM_UPDATE`.
    let fr = unsafe {
        let p = core::ptr::addr_of_mut!(WR_FRUSTUM).cast::<CFrustum>();
        (eng.scene.frustum_init)(p);
        frustum_set(&mut *p, wp.origin, wp.angles, wp.fov, wp.w as f32 / wp.h as f32, wp.znear, wp.zfar);
        (eng.scene.frustum_update)(p);
        p
    };

    // :2967-2970: the per-view render attributes.
    // SAFETY: the block is the C's 1024-byte aligned scratch, zeroed then constructed by the
    // pinned `EFN_ATTRS_CTOR`.
    let attrs = unsafe {
        let block = core::ptr::addr_of_mut!(WR_ATTRS).cast::<AttrsBlock>();
        core::ptr::write_bytes(block.cast::<u8>(), 0, 1024); // :2969
        (eng.engine.attrs_ctor)(core::ptr::addr_of_mut!((*block).attrs));
        block
    };

    // :2972-2981
    let vp = RenderViewport_t {
        m_nVersion: 1,
        m_nTopLeftX: 0,
        m_nTopLeftY: 0,
        m_nWidth: wp.w as i32,
        m_nHeight: wp.h as i32,
        m_flMinZ: 0.0,
        m_flMaxZ: 1.0,
    };
    let rt = RenderTargetDesc {
        w: wp.w as i32,
        h: wp.h as i32,
        msaa: 0,
        pad: 0,
    };
    // CreateViewId leaves bytes 12-15 untouched; the C zeroes its 16-byte, 16-aligned buffer.
    #[repr(align(16))]
    struct AlignedVid([u8; 16]);
    let mut vid = AlignedVid([0u8; 16]);
    let names: [*const c_char; 2] = [c"scratchcolor".as_ptr(), c"scratchdepth".as_ptr()];
    // SAFETY: `rd` passed `obj_is(RV_RENDERDEVICE_VT)`; IsUsingReverseDepth is its pinned slot
    // 168, called through the C header's `u8` signature.
    let reverse_depth = unsafe {
        slot_fn::<FnReverseDepth>(rd.cast(), IRenderDevice_slots::IsUsingReverseDepth)(rd)
    } != 0;
    let fmts: [i32; 2] = [
        WR_COLOR_FMT,
        if reverse_depth {
            IMAGE_FORMAT_D32FS8 as i32
        } else {
            IMAGE_FORMAT_D24S8 as i32
        }, // :2977
    ];
    // SAFETY: `pipe` passed `obj_is(SV_PIPELINE_ALIAS_VT)`; the three flag getters are pinned
    // slots 8/9/10.
    let (bind, tflags) = unsafe {
        let vt = IRenderingPipeline::vtable(pipe);
        // The generated names are the header's anonymous slots: `unk108` is
        // `ScratchTargetFlags` (slot 8), `unk109` `ColorBindFlags` (9), `unk1010`
        // `DepthBindFlags` (10) - `s2sdk.h:212-214`.
        let color = vt.unk109.expect("pinned ColorBindFlags slot")(pipe);
        let depth = vt.unk1010.expect("pinned DepthBindFlags slot")(pipe);
        let flags = vt.unk108.expect("pinned ScratchTargetFlags slot")(pipe);
        ([color, depth], flags | 1) // :2978-2979
    };
    let mut hcol: i64 = -1;
    let mut hdep: i64 = -1;
    let mut outs: [*mut *mut c_void; 2] = [
        (&mut hcol as *mut i64).cast(),
        (&mut hdep as *mut i64).cast(),
    ]; // :2980-2981
    let mut fail: Option<&'static str> = None;
    let mut tex: *mut c_void = core::ptr::null_mut();
    let read: u8;

    // :2988-3015: every pinned vtable entry the bracket calls is bound here, **before**
    // `BeginRenderingViews` opens the frame, so the generated `Option`s' `.expect()`s (a panic in
    // Rust; unreachable after `engine_ifaces`' own slot checks) cannot fire with it open. The
    // scene-view table needs the `ISceneView` `AddView` returns and is bound right after it
    // (`:2996`); `Frame` closes the frame on any panic that still happens in between.
    // SAFETY: `ss` passed `obj_is(SV_SCENESYSTEM_VT)`; its vptr is the pinned class vtable in a
    // verified module's image (read-only for the process lifetime). WaitForRenderingToComplete is
    // slot 26, taken through the C header's `u8` signature.
    let (wait, begin_views, create_view_id, add_view, finish_views) = unsafe {
        let vt = ISceneSystem::vtable(ss);
        (
            slot_fn::<FnWait>(ss.cast(), ISceneSystem_slots::WaitForRenderingToComplete),
            vt.BeginRenderingViews.expect("pinned BeginRenderingViews slot"),
            vt.CreateViewId.expect("pinned CreateViewId slot"),
            vt.AddView.expect("pinned AddView slot"),
            vt.FinishRenderingViews.expect("pinned FinishRenderingViews slot"),
        )
    };
    // SAFETY: `pipe` passed `obj_is(SV_PIPELINE_ALIAS_VT)`; AddLayersToView is slot 4 and
    // `unk103` (GetLayerViewport) slot 3 of that pinned vtable.
    let (add_layers, get_layer_viewport) = unsafe {
        let vt = IRenderingPipeline::vtable(pipe);
        (
            vt.AddLayersToView.expect("pinned AddLayersToView slot"),
            vt.unk103.expect("pinned GetLayerViewport slot"),
        )
    };

    let t0 = now_ms(); // :2986
    log::line("world.render: begin"); // :2987
    // SAFETY: as above.
    unsafe { wait(ss) }; // :2988
    // :2989-2992
    // SAFETY: `ss`/`rd`/`world` all passed their identity checks; `fr`/`vp` are locals that
    // outlive the calls. The frame opens at `begin_views`.
    let mut frame = Frame {
        ss,
        finish: finish_views,
        wait,
        open: false,
    };
    let view = unsafe {
        begin_views(ss, rd, true); // :2990
        frame.open = true;
        create_view_id(ss, vid.0.as_mut_ptr().cast(), 0xFFFF_FFFFu32 as i32); // :2991
        add_view(
            ss,
            c"mcp world.render".as_ptr(),
            vid.0.as_ptr().cast(),
            fr,
            core::ptr::null_mut(),
            world,
            &vp,
            core::ptr::null_mut(),
            65536, // :2992: the default view priority (1 << 16)
        )
    };
    if !obj_is(view.cast(), pins::PIN_SCENESYSTEM, pins::SV_SCENEVIEW_VT.value as u32) {
        fail = Some("AddView failed"); // :2993
    }
    if fail.is_none() {
        log::line("world.render: view"); // :2995
        // SAFETY: `view` passed `obj_is(SV_SCENEVIEW_VT)`; AddFlags/CreateScratchTargets are the
        // pinned slots 24/88 of that vtable, called through the generated typed fields.
        unsafe {
            let vt = ISceneView::vtable(view);
            vt.AddFlags.expect("pinned AddFlags slot")(
                view,
                16 | pins::S2_VIEWFLAG_SIZED_SCRATCH.value as u32, // :2998: TOOLS3D | SIZED_SCRATCH
            );
            vt.CreateScratchTargets.expect("pinned CreateScratchTargets slot")(
                view,
                2,
                names.as_ptr().cast_mut(),
                fmts.as_ptr(),
                bind.as_ptr(),
                outs.as_mut_ptr(),
                wp.w,
                wp.h,
                0,
                tflags,
            ); // :2999
        }
        if hcol == -1 || hdep == -1 {
            fail = Some("scratch target allocation failed"); // :3000
        }
    }
    if fail.is_none() {
        log::line("world.render: layers"); // :3003
        // SAFETY: the handle table is the C's pinned `S2OFF_ATTRS_HANDLE_TABLE` inside the
        // constructed attribute block; the setter is the pinned `EFN_ATTRS_SET_HANDLE`.
        unsafe {
            let table = (core::ptr::addr_of_mut!((*attrs).attrs) as *mut u8)
                .add(pins::S2OFF_ATTRS_HANDLE_TABLE.value as usize)
                .cast::<c_void>();
            (eng.engine.attrs_set_handle)(table, TOKEN_OUTPUT_COLOR, hcol); // :3004
            (eng.engine.attrs_set_handle)(table, TOKEN_OUTPUT_DEPTH, hdep); // :3005
        }
        // SAFETY: `view`/`pipe` passed their identity checks; BeginLayerGroup/AddLayersToView/
        // EndLayerGroup/GetLayerViewport/MarkRenderTargetForReadback are pinned slots.
        unsafe {
            let svt = ISceneView::vtable(view);
            svt.BeginLayerGroup.expect("pinned BeginLayerGroup slot")(view, c"Main".as_ptr()); // :3006
            add_layers(
                pipe,
                view,
                &vp,
                core::ptr::from_ref(&rt).cast_mut().cast(),
                core::ptr::addr_of_mut!((*attrs).attrs),
            ); // :3007
            svt.EndLayerGroup.expect("pinned EndLayerGroup slot")(view); // :3008
            let mut vp_out = core::mem::MaybeUninit::<RenderViewport_t>::uninit();
            get_layer_viewport(
                pipe,
                CUtlStringToken { m_nHashCode: 0 },
                &vp,
                vp_out.as_mut_ptr(),
            ); // :3009-3010
            svt.MarkRenderTargetForReadback
                .expect("pinned MarkRenderTargetForReadback slot")(
                view, hcol as usize as *mut c_void,
            ); // :3011
        }
    }
    log::line("world.render: finish"); // :3013
    // The C's own finish side; disarm first so the guard cannot run it twice, then close.
    frame.open = false;
    // SAFETY: as above; FinishRenderingViews is pinned slot 25 (FLT_MAX = "use the platform
    // time") and the final wait is slot 26.
    unsafe {
        finish_views(ss, f32::MAX); // :3014
        wait(ss); // :3015
    }
    let t1 = now_ms(); // :3016
    if fail.is_none() {
        // SAFETY: `view` passed its identity check; GetRenderTargetTexture is pinned slot 45,
        // called with the handle the scratch allocation wrote. Its Windows shape takes the
        // out-pointer the C passes (`void **outTexture`).
        let got = unsafe {
            ISceneView::vtable(view)
                .GetRenderTargetTexture
                .expect("pinned GetRenderTargetTexture slot")(view, (&raw mut tex).cast(), hcol as usize as *mut c_void)
        };
        let _ = got; // :3018: the C reads the out-pointer, not the return
        if tex.is_null() {
            fail = Some("colour target has no texture"); // :3019
        }
    }
    if fail.is_none() {
        log::line("world.render: readback"); // :3022
        let rect = Rect {
            x: 0,
            y: 0,
            w: wp.w as i32,
            h: wp.h as i32,
        }; // :3023
        // SAFETY: `rd` passed its identity check; ReadTexturePixels is pinned slot 74, called
        // through the C header's `u8` signature with the live `px` buffer and its stride.
        read = unsafe {
            slot_fn::<FnReadPixels>(rd.cast(), IRenderDevice_slots::ReadTexturePixels)(
                rd,
                tex,
                core::ptr::from_ref(&rect).cast_mut(),
                0,
                0,
                core::ptr::from_ref(&rect).cast_mut(),
                px.as_mut_ptr().cast(),
                IMAGE_FORMAT_RGBA8888 as i32,
                (wp.w * 4) as i32,
            )
        }; // :3024
        if read == 0 {
            fail = Some("ReadTexturePixels failed"); // :3025
        }
    }
    // SAFETY: the block was constructed by the pinned ctor above, exactly once.
    unsafe {
        (eng.engine.attrs_dtor)(core::ptr::addr_of_mut!((*attrs).attrs));
    } // :3027
    let t2 = now_ms(); // :3028
    if let Some(reason) = fail {
        log::line("world.render: failed"); // :3031
        return Err(OpErr::new(-32020, "RenderFailed", reason)); // :3032
    }

    let (raw, stats) = convert_word_pixels(&px, wp.w, wp.h, wp.srgb); // :3036-3054
    drop(px);
    if stats.untouched == npx {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            "readback did not run (no pixels written)", // :3058
        ));
    }
    let crc = crc32(&raw); // :3061-3062
    let png = png_encode(&raw, wp.w, wp.h)
        .ok_or_else(|| OpErr::new(-32020, "RenderFailed", "png encode: out of memory"))?; // :3063-3066
    drop(raw);

    let dir = renders_dir();
    ensure_renders_dir(&dir); // :3067
    // the C's SB was 200 bytes; a native dir is longer. 290 + `.tmp` still fits
    // write_file_atomic's 300-byte temp name
    let mut pathbuf = vec![0u8; 290];
    let mut ps = Sb::new(&mut pathbuf);
    ps.add(&dir); // :3068-3069
    ps.add(&wp.stem);
    ps.add(b".png");
    let wrote = !ps.ovf() && write_file_atomic(ps.as_cstr(), &png);
    let path = ps.as_cstr().to_vec();
    let t3 = now_ms(); // :3072
    if !wrote {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            &write_failed_reason(&dir), // :3073
        ));
    }
    log::line("world.render: wrote"); // :3074
    log::line(&String::from_utf8_lossy(&path)); // :3075

    let mut fbuf = vec![0u8; 96];
    let mut fs = Sb::new(&mut fbuf);
    fs.add(&wp.stem); // :3079-3080
    fs.add(b".png");
    let file = fs.as_cstr().to_vec();

    let reply = WorldReply {
        path,
        file,
        width: wp.w,
        height: wp.h,
        bytes: png.len() as u64,
        origin: wp.origin,
        angles: wp.angles,
        fov: wp.fov,
        znear: wp.znear,
        zfar: wp.zfar,
        srgb: wp.srgb,
        stats,
        crc,
        render_ms: t1 - t0, // :3100-3102
        readback_ms: t2 - t1,
        encode_ms: t3 - t2,
    };
    world_reply_into(out, &reply);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{world_reply_into, WorldReply};
    use crate::entity_json::Sb;
    use crate::ops::render::Stats;

    /// The whole result object, byte for byte from `mcp_bridge.c:3081-3104`.
    #[test]
    fn world_reply_is_the_c_object() {
        let reply = WorldReply {
            path: b"Z:\\app\\renders\\w.png".to_vec(),
            file: b"w.png".to_vec(),
            width: 320,
            height: 180,
            bytes: 456,
            origin: [1.0, 2.0, 3.0],
            angles: [4.0, 5.0, 6.0],
            fov: 90.0,
            znear: 4.0,
            zfar: 30000.0,
            srgb: false,
            stats: Stats {
                untouched: 0,
                luma_sum: 57600,
                non_black: 57600,
                luma_min: 1,
                luma_max: 1,
            },
            crc: 0x0000_00AB,
            render_ms: 1,
            readback_ms: 2,
            encode_ms: 3,
        };
        let mut buf = vec![0u8; 1024];
        let mut out = Sb::new(&mut buf);
        world_reply_into(&mut out, &reply);
        assert!(!out.ovf());
        assert_eq!(
            std::str::from_utf8(out.as_cstr()).unwrap(),
            r#"{"path":"Z:\\app\\renders\\w.png","file":"w.png","width":320,"height":180,"bytes":456,"camera":{"origin":[1,2,3],"angles":[4,5,6],"fov":90,"znear":4,"zfar":30000},"pipeline":"Tools->CSGO","srgb":false,"stats":{"untouchedPixels":0,"nonBlackPixels":57600,"meanLuma":1,"minLuma":1,"maxLuma":1,"crc32":"000000ab"},"ms":{"render":1,"readback":2,"encode":3}}"#
        );
    }
}
