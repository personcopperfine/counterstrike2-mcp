//! The shared `pet.render` / `modeldoc.render` tail (`bridge/mcp_bridge_pet_render.inc:35-167`).
//!
//! Both ops are the same call on the same statically linked tools-lib function, one copy per
//! module: `CToolScene::Render(scene, camera, IToolSceneWindow *wnd|0, swapchain|0,
//! const RenderViewport_t *vp (w, h), u8 *pixels|0, int imageFormat)`. With `wnd = 0` it builds a
//! `COffscreenToolSceneWindow` from the camera, runs the scene system's Begin/Finish/Wait bracket
//! itself and reads the colour target back with the render device's `ReadTexturePixels`
//! (row pitch `w * 4`, RGBA8888). The pixels are display-ready (the widget shows them as a
//! `QImage` unchanged), so the stats loop applies **no** sRGB LUT here, unlike `view.render`.
//!
//! The tail after the chain - the sentinel buffer, the Render call, the convert + stats pass,
//! CRC, PNG encode, the atomic write and the result JSON - is shared, so the wire result is
//! byte-identical for both ops (`mcp_bridge_pet_render.inc:18-22`).
//!
//! [`render_png_with`] takes the writer and the clock as parameters so the host tests can drive
//! the whole tail (its reply bytes included) without a game or a filesystem.

use core::ffi::c_void;

use s2sdk_sys::generated::RenderViewport_t;

use crate::entity_json::Sb;
use crate::log;
use crate::ops::OpErr;

use super::{
    add_stats, convert_word_pixels, crc32, ensure_renders_dir, fill_sentinel_u32, now_ms,
    png_encode, renders_dir, write_failed_reason, write_file_atomic, Stats,
};

/// `ToolSceneRenderFn` (`mcp_bridge_pet_render.inc:35-36`).
pub(crate) type ToolSceneRenderFn = unsafe extern "C" fn(
    scene: *mut c_void,
    camera: *mut c_void,
    wnd: *mut c_void,
    swapchain: *mut c_void,
    vp: *const RenderViewport_t,
    pixels: *mut u8,
    image_format: i32,
);

/// `tag_log` (`mcp_bridge_pet_render.inc:40-45`): `<tag>: <what>`.
pub(crate) fn tag_log(tag: &str, what: &str) {
    log::line(&format!("{tag}: {what}"));
}

/// `tag_lognum` (`mcp_bridge_pet_render.inc:46-51`): `<tag>: <what><decimal>`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn tag_lognum(tag: &str, what: &str, value: u64) {
    log::line(&format!("{tag}: {what}{value}"));
}

/// The `pet.render` / `modeldoc.render` result's inputs (`mcp_bridge_pet_render.inc:145-165`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolSceneReply {
    pub path: Vec<u8>,
    pub file: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub has_document: bool,
    pub stats: Stats,
    pub crc: u32,
    pub duration_ms: u64,
}

/// The shared result object (`mcp_bridge_pet_render.inc:149-165`), byte for byte.
pub(crate) fn toolscene_reply_into(out: &mut Sb, r: &ToolSceneReply) {
    out.add(b"{\"path\":"); // :149
    out.addjson(&r.path);
    out.add(b",\"file\":");
    out.addjson(&r.file);
    out.add(b",\"width\":");
    out.addnum(u64::from(r.width));
    out.add(b",\"height\":");
    out.addnum(u64::from(r.height));
    out.add(b",\"bytes\":");
    out.addnum(r.bytes);
    out.add(b",\"format\":\"RGBA8888\""); // :154
    out.add(b",\"hasDocument\":");
    out.add(if r.has_document { b"true" } else { b"false" }); // :155
    add_stats(out, &r.stats, u64::from(r.width) * u64::from(r.height), r.crc); // :156-163
    out.add(b",\"durationMs\":"); // :164
    out.addnum(r.duration_ms);
    out.add(b"}");
}

/// `toolscene_render_png` (`mcp_bridge_pet_render.inc:78-167`): the caller has already validated
/// the parameters and resolved `(scene, camera)`; this renders, converts, encodes, writes and
/// builds the reply.
// The C passes these one by one (`toolscene_render_png`, `mcp_bridge_pet_render.inc:78-80`); the
// port keeps the same parameter list so a call site reads like the C's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_png_with(
    tag: &str,
    render: ToolSceneRenderFn,
    scene: *mut c_void,
    camera: *mut c_void,
    has_document: bool,
    w: u32,
    h: u32,
    stem: &[u8],
    out: &mut Sb,
    write: &dyn Fn(&[u8], &[u8]) -> bool,
    now: &dyn Fn() -> u64,
) -> Result<(), OpErr> {
    let npx = u64::from(w) * u64::from(h);
    let mut px = vec![0u8; (npx * 4) as usize]; // :82
    fill_sentinel_u32(&mut px); // :90

    let vp = RenderViewport_t {
        m_nVersion: 1,
        m_nTopLeftX: 0,
        m_nTopLeftY: 0,
        m_nWidth: w as i32,
        m_nHeight: h as i32,
        m_flMinZ: 0.0,
        m_flMaxZ: 1.0,
    }; // :92
    let t0 = now(); // :93
    tag_log(tag, "begin"); // :94
    // SAFETY: `scene`/`camera` were identity-checked by the caller's chain and `render` is the
    // module's own pinned `CToolScene::Render` RVA; `px` holds `npx * 4` writable bytes and `vp`
    // outlives the call.
    unsafe { render(scene, camera, core::ptr::null_mut(), core::ptr::null_mut(), &vp, px.as_mut_ptr(), 0) };
    tag_log(tag, "finish"); // :96

    // :98-126: convert + stats; the pixels are display-ready (no sRGB LUT).
    let (raw, stats) = convert_word_pixels(&px, w, h, false);
    drop(px);
    if stats.untouched == npx {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            "readback did not run (no pixels written)", // :124
        ));
    }
    let crc = crc32(&raw); // :127-128
    let png = png_encode(&raw, w, h)
        .ok_or_else(|| OpErr::new(-32020, "RenderFailed", "png encode: out of memory"))?; // :129-132
    drop(raw);

    let dir = renders_dir();
    ensure_renders_dir(&dir); // :134
    // the C's SB was 200 bytes; a native dir is longer. 290 + `.tmp` still fits
    // write_file_atomic's 300-byte temp name
    let mut pathbuf = vec![0u8; 290];
    let mut ps = Sb::new(&mut pathbuf);
    ps.add(&dir); // :136-137
    ps.add(stem);
    ps.add(b".png");
    let wrote = !ps.ovf() && write(ps.as_cstr(), &png);
    let path = ps.as_cstr().to_vec();
    let t2 = now(); // :140
    if !wrote {
        return Err(OpErr::new(
            -32020,
            "RenderFailed",
            &write_failed_reason(&dir), // :141
        ));
    }
    tag_log(tag, "wrote"); // :142
    log::line(&String::from_utf8_lossy(&path)); // :143

    let mut fbuf = vec![0u8; 96]; // :146-148
    let mut fs = Sb::new(&mut fbuf);
    fs.add(stem);
    fs.add(b".png");
    let file = fs.as_cstr().to_vec();

    let reply = ToolSceneReply {
        path,
        file,
        width: w,
        height: h,
        bytes: png.len() as u64,
        has_document,
        stats,
        crc,
        duration_ms: t2 - t0, // :164
    };
    toolscene_reply_into(out, &reply);
    Ok(())
}

/// The production tail: the Win32 atomic write and the process clock.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_png(
    tag: &str,
    render: ToolSceneRenderFn,
    scene: *mut c_void,
    camera: *mut c_void,
    has_document: bool,
    w: u32,
    h: u32,
    stem: &[u8],
    out: &mut Sb,
) -> Result<(), OpErr> {
    render_png_with(tag, render, scene, camera, has_document, w, h, stem, out, &write_file_atomic, &now_ms)
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::ffi::c_void;

    use s2sdk_sys::generated::RenderViewport_t;

    use super::{render_png_with, toolscene_reply_into, ToolSceneReply};
    use crate::entity_json::Sb;
    use crate::ops::render::Stats;

    /// The shared result object, byte for byte from `mcp_bridge_pet_render.inc:149-165`.
    #[test]
    fn toolscene_reply_is_the_c_object() {
        let reply = ToolSceneReply {
            path: b"Z:\\app\\renders\\p.png".to_vec(),
            file: b"p.png".to_vec(),
            width: 320,
            height: 240,
            bytes: 789,
            has_document: false,
            stats: Stats {
                untouched: 0,
                luma_sum: 384_000, // 5.0 over the 320x240 reply pixels
                non_black: 3,
                luma_min: 2,
                luma_max: 8,
            },
            crc: 0,
            duration_ms: 42,
        };
        let mut buf = vec![0u8; 1024];
        let mut out = Sb::new(&mut buf);
        toolscene_reply_into(&mut out, &reply);
        assert_eq!(
            std::str::from_utf8(out.as_cstr()).unwrap(),
            r#"{"path":"Z:\\app\\renders\\p.png","file":"p.png","width":320,"height":240,"bytes":789,"format":"RGBA8888","hasDocument":false,"stats":{"untouchedPixels":0,"nonBlackPixels":3,"meanLuma":5,"minLuma":2,"maxLuma":8,"crc32":"00000000"},"durationMs":42}"#
        );
    }

    /// A stand-in `CToolScene::Render` that fills every pixel with RGBA (1,2,3,4).
    unsafe extern "C" fn fill_pixels(
        _scene: *mut c_void,
        _camera: *mut c_void,
        _wnd: *mut c_void,
        _swapchain: *mut c_void,
        vp: *const RenderViewport_t,
        pixels: *mut u8,
        image_format: i32,
    ) {
        assert_eq!(image_format, 0, "the tail always asks for RGBA8888");
        // SAFETY: the caller passes a viewport and a `w * h * 4` buffer.
        let vp = unsafe { *vp };
        let npx = (vp.m_nWidth * vp.m_nHeight) as usize;
        // SAFETY: as above.
        let px = unsafe { core::slice::from_raw_parts_mut(pixels, npx * 4) };
        let (words, _rest) = px.as_chunks_mut::<4>();
        for word in words {
            word.copy_from_slice(&[1, 2, 3, 4]);
        }
    }

    /// A stand-in render that never writes: every pixel keeps the sentinel.
    unsafe extern "C" fn leave_sentinel(
        _scene: *mut c_void,
        _camera: *mut c_void,
        _wnd: *mut c_void,
        _swapchain: *mut c_void,
        _vp: *const RenderViewport_t,
        _pixels: *mut u8,
        _image_format: i32,
    ) {
    }

    /// The whole tail: the render call, the stats, the PNG, the atomic write and the reply - the
    /// wire bytes and the bytes handed to the writer.
    #[test]
    fn render_tail_writes_the_c_result() {
        let written: Cell<usize> = Cell::new(0);
        let clock = Cell::new(100u64);
        let path: core::cell::RefCell<Vec<u8>> = core::cell::RefCell::new(Vec::new());
        let png: core::cell::RefCell<Vec<u8>> = core::cell::RefCell::new(Vec::new());
        let write = |p: &[u8], data: &[u8]| {
            *path.borrow_mut() = p.to_vec();
            *png.borrow_mut() = data.to_vec();
            written.set(written.get() + 1);
            true
        };
        let now = || {
            let t = clock.get();
            clock.set(t + 25);
            t
        };
        let mut buf = vec![0u8; 1024];
        let mut out = Sb::new(&mut buf);
        let outcome = render_png_with(
            "pet.render",
            fill_pixels,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            true,
            16,
            8,
            b"t",
            &mut out,
            &write,
            &now,
        );
        assert!(outcome.is_ok());
        assert_eq!(written.get(), 1);
        assert_eq!(*path.borrow(), b"Z:\\app\\renders\\t.png"); // :136-137
        let text = std::str::from_utf8(out.as_cstr()).unwrap();
        // luma of (1,2,3) is (54 + 366 + 57) >> 8 == 1 for every pixel, none above 8.
        assert!(text.contains(r#""width":16,"height":8"#));
        assert!(text.contains(r#""format":"RGBA8888","hasDocument":true"#));
        assert!(text.contains(r#""untouchedPixels":0,"nonBlackPixels":0,"meanLuma":1,"minLuma":1,"maxLuma":1"#));
        assert!(text.contains(r#""durationMs":25"#)); // t2 (125, the clock returned 100 then 125) - t0

        // The PNG decodes to the 16x8 RGB image the tail built.
        let png = png.borrow();
        let decoder = png::Decoder::new(std::io::Cursor::new(&*png));
        let mut reader = decoder.read_info().expect("IHDR");
        let mut pixels = vec![0u8; reader.output_buffer_size().expect("bounded")];
        let info = reader.next_frame(&mut pixels).expect("IDAT");
        assert_eq!((info.width, info.height), (16, 8));
        assert_eq!(&pixels[..3], &[1, 2, 3]);
        assert_eq!(&pixels[info.buffer_size() - 3..info.buffer_size()], &[1, 2, 3]);
    }

    /// A render that never wrote a pixel is the C's `readback did not run` error, and nothing is
    /// written (`mcp_bridge_pet_render.inc:122-126`).
    #[test]
    fn render_tail_refuses_an_untouched_buffer() {
        let wrote = Cell::new(false);
        let write = |_: &[u8], _: &[u8]| {
            wrote.set(true);
            true
        };
        let mut buf = vec![0u8; 512];
        let mut out = Sb::new(&mut buf);
        let err = render_png_with(
            "pet.render",
            leave_sentinel,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            false,
            16,
            8,
            b"t",
            &mut out,
            &write,
            &|| 0,
        )
        .unwrap_err();
        assert_eq!((err.code, err.name), (-32020, "RenderFailed"));
        assert_eq!(err.data, r#"{"reason":"readback did not run (no pixels written)"}"#);
        assert!(!wrote.get(), "a refused render must not write");
    }

    /// A failing write is the C's `could not write` error (`:140-141`).
    #[test]
    fn render_tail_reports_a_failed_write() {
        let mut buf = vec![0u8; 512];
        let mut out = Sb::new(&mut buf);
        let err = render_png_with(
            "modeldoc.render",
            fill_pixels,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            false,
            16,
            8,
            b"t",
            &mut out,
            &|_, _| false,
            &|| 0,
        )
        .unwrap_err();
        assert_eq!(err.data, r#"{"reason":"could not write Z:\\app\\renders"}"#);
    }
}
