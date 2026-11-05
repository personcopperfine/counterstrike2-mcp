//! The four render-to-PNG methods: `view.render`, `world.render`, `pet.render`,
//! `modeldoc.render` (M4 lane RENDER).
//!
//! Ported from the frozen C bridge (`bridge/mcp_bridge.c:2500-3134`, the C's render statics and
//! the two Hammer/engine ops) and its two includes (`bridge/mcp_bridge_pet_render.inc`,
//! `bridge/mcp_bridge_modeldoc_render.inc`). Everything the four share lives here:
//!
//! * the PNG writer (`crc_init` / `crc_update` / `put_be32` / `png_encode`, `:2533-2601`),
//! * the atomic file write (`write_file_atomic`, `:2602-2622`),
//! * the output-name rule (`render_name`, `:2624-2646`),
//! * `sb_addf3` (`:2648-2655`), `json_bool` (`:2525-2531`), the sRGB LUT (`:2519`) and
//!   `frustum_set` (`:2657-2666`),
//! * the pixel convert + stats pass every op runs (`:2767-2791`, `:3036-3054`,
//!   `mcp_bridge_pet_render.inc:98-120`),
//! * the `pet.render` / `modeldoc.render` parameter contract and shared tail
//!   (`mcp_bridge_pet_render.inc:53-167`).
//!
//! The four bodies are in [`view`], [`world`], [`pet`] and [`modeldoc`]; [`toolscene`] holds the
//! shared ToolScene tail those last two call. Every result builder is a pure function over plain
//! inputs (the small [`Stats`] / [`CameraFacts`] structs a thin reader fills), so the host tests
//! hold each one to the C's `sb_add` sequence byte for byte.
//!
//! # Buffer sizes
//!
//! The C's dispatch arms differ per method and the port keeps each: `world.render` and
//! `view.render` build into a 4096-byte `rbuf` and **do not check `ovf`** (`:6051-6077`; a
//! truncated object is appended as-is), while `pet.render` and `modeldoc.render` go through the
//! pet (1 MiB - 2048) and modeldoc (256 KiB) arms and answer `-32603` on overflow. The render
//! replies are ~600 bytes, so the difference is only visible on a pathological `name`.
//!
//! # Statics
//!
//! `g_renderSeq` (`:2522`) is a `static` counter, `g_vrFrustum` / `g_wrFrustum` /
//! `g_wrAttrs` are process-wide aligned scratch blocks, and the C's CRC table is built once.
//! All of them are touched on the game's main thread only (the tick's drain), like the C; the
//! Rust port keeps the same shape (an atomic counter, `static mut` aligned scratch read through
//! `addr_of_mut!`, and a `OnceLock` table).

pub mod modeldoc;
pub mod pet;
pub mod toolscene;
pub mod view;
pub mod world;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::entity_json::{
    json_find, json_float1, json_float3, json_int, json_string, Sb,
};
use crate::ops::{Handler, OpErr};

/// `VR_MAX_SIDE` (`mcp_bridge.c:2512`): the largest side `view.render` / the ToolScene ops take.
pub(crate) const VR_MAX_SIDE: i64 = 4096;
/// `VR_MAX_PIXELS` (`mcp_bridge.c:2513`).
pub(crate) const VR_MAX_PIXELS: u64 = 4096 * 2048;
/// `VR_SENTINEL` (`mcp_bridge.c:2514`): written into every pixel before the readback so a render
/// that never wrote can be told apart from a black one.
pub(crate) const VR_SENTINEL: u32 = 0xA55A_5AA5;
/// `WR_MAX_SIDE` (`mcp_bridge.c:2868`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WR_MAX_SIDE: i64 = 8192;
/// `WR_MAX_PIXELS` (`mcp_bridge.c:2869`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WR_MAX_PIXELS: u64 = 8192 * 4096;

/// The four handlers. The C's dispatch chain reaches them at `mcp_bridge.c:6051` (world),
/// `:6063` (view), `:5220` (modeldoc, through `op_modeldoc`) and `:6222` (pet, through
/// `op_pet`).
pub static HANDLERS: &[(&str, Handler)] = &[
    ("view.render", view::handler),
    ("world.render", world::handler),
    ("pet.render", pet::handler),
    ("modeldoc.render", modeldoc::handler),
];

// ---------------------------------------------------------------------------------------------
// The C's statics and shared helpers (`mcp_bridge.c:2516-2666`)
// ---------------------------------------------------------------------------------------------

/// `k_srgbLut` (`mcp_bridge.c:2519`): Hammer's `LinearToSRGB` at `i/255`, truncated - exactly
/// what the thumbnail/screenshot loop applies.
pub(crate) const SRGB_LUT: [u8; 256] = [
    0, 12, 21, 28, 33, 38, 42, 46, 49, 52, 55, 58, 61, 63, 66, 68, 70, 73, 75, 77, 79, 81, 82,
    84, 86, 88, 89, 91, 93, 94, 96, 97, 99, 100, 102, 103, 104, 106, 107, 109, 110, 111, 112,
    114, 115, 116, 117, 118, 120, 121, 122, 123, 124, 125, 126, 127, 129, 130, 131, 132, 133,
    134, 135, 136, 137, 138, 139, 140, 141, 142, 142, 143, 144, 145, 146, 147, 148, 149, 150,
    151, 151, 152, 153, 154, 155, 156, 157, 157, 158, 159, 160, 161, 161, 162, 163, 164, 165,
    165, 166, 167, 168, 168, 169, 170, 171, 171, 172, 173, 174, 174, 175, 176, 176, 177, 178,
    179, 179, 180, 181, 181, 182, 183, 183, 184, 185, 185, 186, 187, 187, 188, 189, 189, 190,
    191, 191, 192, 193, 193, 194, 194, 195, 196, 196, 197, 197, 198, 199, 199, 200, 201, 201,
    202, 202, 203, 204, 204, 205, 205, 206, 206, 207, 208, 208, 209, 209, 210, 210, 211, 212,
    212, 213, 213, 214, 214, 215, 215, 216, 217, 217, 218, 218, 219, 219, 220, 220, 221, 221,
    222, 222, 223, 223, 224, 224, 225, 226, 226, 227, 227, 228, 228, 229, 229, 230, 230, 231,
    231, 232, 232, 233, 233, 234, 234, 235, 235, 236, 236, 237, 237, 237, 238, 238, 239, 239,
    240, 240, 241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 245, 246, 246, 247, 247, 248,
    248, 249, 249, 250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254, 254,
];

/// `g_renderSeq` (`mcp_bridge.c:2522`): the default-name counter, bumped only in
/// `render_name`'s default branch (`:2643`). Main thread only.
static RENDER_SEQ: AtomicU64 = AtomicU64::new(0);

/// `++g_renderSeq` (`mcp_bridge.c:2643`): the default branch of `render_name`, after every
/// earlier validation - never for an explicit name and never for a refused one.
fn next_seq() -> u64 {
    RENDER_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// `GetTickCount64()` as `render_name` uses it (`mcp_bridge.c:2645`): **milliseconds since boot**,
/// which the C's default name carries (`render_<tick>_<seq>`). hammer-mcp relies on that
/// uniqueness across bridge restarts (`hammer-mcp/src/tools/render.rs:490-494`), so a
/// process-relative clock would not do here; only the `ms.*`/`durationMs` deltas use [`now_ms`].
#[cfg(windows)]
fn name_tick() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    // SAFETY: `GetTickCount64` has no preconditions; its return is the boot-relative ms count.
    unsafe { GetTickCount64() }
}

/// The non-Windows name tick: `CLOCK_BOOTTIME`, the same quantity as `GetTickCount64` (monotonic
/// since boot, including suspend), via the crate's existing `libc` dependency.
#[cfg(target_os = "linux")]
fn name_tick() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live `timespec` and `CLOCK_BOOTTIME` is a valid clock id on Linux.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0 {
        return ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000;
    }
    system_ms()
}

/// A host with neither `GetTickCount64` nor `CLOCK_BOOTTIME`: wall-clock milliseconds since the
/// epoch. Still monotonic enough for the name's uniqueness within and across processes; only the
/// default NAME uses it (every reported delta is [`now_ms`]'s).
#[cfg(not(any(windows, target_os = "linux")))]
fn name_tick() -> u64 {
    system_ms()
}

#[cfg(not(windows))]
fn system_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `GetTickCount64()` in the `ms.*` / `durationMs` deltas (`mcp_bridge.c:2761`, `:2986`,
/// `mcp_bridge_pet_render.inc:93`): only differences are reported, so the process's monotonic
/// clock answers the same deltas.
fn now_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// `json_bool` (`mcp_bridge.c:2525-2531`): the depth-blind finder's boolean. `None` is the C's
/// `0` (not a bool literal there); the caller keeps its default.
///
/// The C compares `p[0..4]`/`p[0..5]` without a bounds check; the port stops at the end of the
/// buffer instead (the only difference is an OOB read the C performs past the terminating NUL).
#[cfg_attr(not(windows), allow(dead_code))]
fn json_bool(json: &[u8], key: &[u8]) -> Option<bool> {
    let start = json_find(json, key)?;
    let end = json.iter().position(|&b| b == 0).unwrap_or(json.len());
    let rest = json.get(start..end)?;
    if rest.starts_with(b"true") {
        Some(true)
    } else if rest.starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

/// `crc_init`'s table (`mcp_bridge.c:2533-2541`), built once.
fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, slot) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        table
    })
}

/// `crc_update` (`mcp_bridge.c:2542-2545`).
pub(crate) fn crc_update(mut c: u32, data: &[u8]) -> u32 {
    for &byte in data {
        c = crc_table()[usize::from((c ^ u32::from(byte)) as u8)] ^ (c >> 8);
    }
    c
}

/// The C's `crc_update(0xFFFFFFFF, ..) ^ 0xFFFFFFFF`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// `put_be32` (`mcp_bridge.c:2546`).
fn put_be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// `png_encode` (`mcp_bridge.c:2550-2601`): an RGB8 PNG with stored (uncompressed) deflate
/// blocks. `raw` is `h` rows of `{filter 0, w*3 bytes}`; `None` is the C's `0` (only its
/// `zlen > 0x7FFFFFFF` refusal - allocation failure is not modelled in Rust).
pub(crate) fn png_encode(raw: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let raw_len = u64::from(h) * (1 + 3 * u64::from(w));
    let nblk = raw_len.div_ceil(65535);
    let zlen = 2 + raw_len + 5 * nblk + 4;
    if zlen > 0x7FFF_FFFF {
        return None;
    }
    let total = 8 + 25 + (12 + zlen) + 12;
    let mut png: Vec<u8> = Vec::with_capacity(total as usize);
    // The table walk below indexes `raw` by the C's computed length; a caller that hands a short
    // buffer is a port bug, so it panics here rather than reading zeroes.
    let raw = &raw[..raw_len as usize];

    // signature (`:2558-2560`)
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR (`:2562-2569`)
    put_be32(&mut png, 13);
    let ihdr = png.len();
    png.extend_from_slice(b"IHDR");
    put_be32(&mut png, w);
    put_be32(&mut png, h);
    png.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, filter 0, no interlace
    let crc = crc32(&png[ihdr..]);
    put_be32(&mut png, crc);

    // IDAT (`:2570-2594`)
    put_be32(&mut png, zlen as u32);
    let idat = png.len();
    png.extend_from_slice(b"IDAT");
    png.extend_from_slice(&[0x78, 0x01]); // zlib: deflate, 32K window, no dict
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    let mut off = 0u64;
    while off < raw_len {
        let n = (raw_len - off).min(65535);
        let final_block = off + n >= raw_len;
        png.push(u8::from(final_block)); // BFINAL, BTYPE=00 (stored)
        png.push(n as u8);
        png.push((n >> 8) as u8);
        let inv = !(n as u16);
        png.push(inv as u8);
        png.push((inv >> 8) as u8);
        for &c in &raw[off as usize..(off + n) as usize] {
            png.push(c);
            s1 += u32::from(c);
            if s1 >= 65521 {
                s1 -= 65521;
            }
            s2 += s1;
            if s2 >= 65521 {
                s2 -= 65521;
            }
        }
        off += n;
    }
    put_be32(&mut png, (s2 << 16) | s1);
    let crc = crc32(&png[idat..]);
    put_be32(&mut png, crc);

    // IEND (`:2596-2600`)
    put_be32(&mut png, 0);
    let iend = png.len();
    png.extend_from_slice(b"IEND");
    let crc = crc32(&png[iend..]);
    put_be32(&mut png, crc);
    Some(png)
}

/// The Wine/Proton container's renders directory (`mcp_bridge.c:2807`), with the trailing `\`.
pub(crate) const WINE_RENDERS_DIR: &[u8] = b"Z:\\app\\renders\\";

/// The directory every render PNG is written to, with a trailing `\`.
///
/// The C hard-codes `Z:\app\renders` (the container's `/app` mount); a native Windows install
/// has no `Z:\app`, and every render failed there (`could not write Z:\app\renders`, seen on the
/// test VM). The rule ([`resolve_renders_dir`]): `CS2_BRIDGE_RENDERS` when set, else
/// `Z:\app\renders\` when `Z:\app` exists (the container, unchanged), else
/// `%LOCALAPPDATA%\cs2mcpbridge\renders\` (next to the discovery file). The reply's `path` is
/// the absolute path, so a server on the same host reads it back as it is.
///
/// A test build always answers the Wine directory: the cases pin the container's bytes and must
/// not depend on the developer's drives or environment.
#[cfg(not(test))]
pub(crate) fn renders_dir() -> Vec<u8> {
    let explicit = std::env::var("CS2_BRIDGE_RENDERS").ok();
    let local = std::env::var("LOCALAPPDATA").ok();
    resolve_renders_dir(
        explicit.as_deref(),
        std::path::Path::new("Z:\\app").is_dir(),
        local.as_deref(),
    )
}

/// [`renders_dir`] in a test build: the Wine directory, always.
#[cfg(test)]
pub(crate) fn renders_dir() -> Vec<u8> {
    WINE_RENDERS_DIR.to_vec()
}

/// [`renders_dir`]'s rule with the environment passed in. An empty value falls through to the
/// next rule; any other one is used as it is (UTF-8, written through the wide-char file APIs in
/// [`write_file_atomic`], so `C:\Users\Jörg\...` works).
pub(crate) fn resolve_renders_dir(
    explicit: Option<&str>,
    z_app_exists: bool,
    localappdata: Option<&str>,
) -> Vec<u8> {
    let usable = |v: &&str| !v.is_empty();
    let with_sep = |dir: &str| {
        let mut out = dir.trim_end_matches(['\\', '/']).as_bytes().to_vec();
        out.push(b'\\');
        out
    };
    if let Some(dir) = explicit.filter(usable) {
        return with_sep(dir);
    }
    if z_app_exists {
        return WINE_RENDERS_DIR.to_vec();
    }
    if let Some(local) = localappdata.filter(usable) {
        let mut out = with_sep(local);
        out.extend_from_slice(b"cs2mcpbridge\\renders\\");
        return out;
    }
    WINE_RENDERS_DIR.to_vec()
}

/// `could not write <dir>` (the C's `could not write Z:\app\renders`, `mcp_bridge.c:2811`).
pub(crate) fn write_failed_reason(dir: &[u8]) -> String {
    let dir = String::from_utf8_lossy(dir);
    format!("could not write {}", dir.trim_end_matches('\\'))
}

/// `CreateDirectoryA("Z:\\app\\renders", 0)` (`mcp_bridge.c:2805`) for [`renders_dir`]'s answer:
/// the result is ignored, as in the C (the write reports the failure). `create_dir_all`, because
/// `CS2_BRIDGE_RENDERS` and `%LOCALAPPDATA%\cs2mcpbridge` may not exist yet.
pub(crate) fn ensure_renders_dir(dir: &[u8]) {
    #[cfg(windows)]
    if let Ok(dir) = std::str::from_utf8(dir) {
        let _ = std::fs::create_dir_all(dir.trim_end_matches('\\'));
    }
    #[cfg(not(windows))]
    let _ = dir;
}

/// `write_file_atomic` (`mcp_bridge.c:2602-2622`): `path + ".tmp"` (built in the C's 300-byte
/// SB), `CREATE_ALWAYS`, 64 MiB write chunks, close, then `MoveFileEx(REPLACE_EXISTING)`.
///
/// `path` is UTF-8 (the renders directory comes from the environment, [`renders_dir`]) and goes
/// through the wide-char file APIs, so a non-ASCII `%LOCALAPPDATA%` (a user name like `Jörg`)
/// works; the C's ANSI calls would have read those bytes in the system code page (FIXME 16).
/// The share mode (0) and flags are the C's. There is no equivalent on a host without the tools
/// filesystem, where every render body is unreachable behind the gate.
#[cfg(windows)]
pub(crate) fn write_file_atomic(path: &[u8], data: &[u8]) -> bool {
    use std::io::Write as _;

    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DeleteFileW, MoveFileExW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
        MOVEFILE_REPLACE_EXISTING,
    };

    // char tmp[300]; SB t; sb_add(path); sb_add(".tmp"); if (t.ovf) return 0;
    let mut tmpbuf = vec![0u8; 300];
    let mut t = Sb::new(&mut tmpbuf);
    t.add(path);
    t.add(b".tmp");
    if t.ovf() {
        return false; // :2606
    }
    let wide = |bytes: &[u8]| -> Option<Vec<u16>> {
        let text = std::str::from_utf8(bytes).ok()?;
        if text.contains('\0') {
            return None;
        }
        Some(std::ffi::OsStr::new(text).encode_wide().chain([0]).collect())
    };
    let Some(tmp_w) = wide(t.as_cstr()) else {
        return false;
    };
    let Some(path_w) = wide(path) else {
        return false;
    };

    // SAFETY: `tmp_w` is a NUL-terminated wide path that outlives the call; the handle is
    // checked before use and closed (or owned by `file`) on every path below.
    let handle = unsafe {
        CreateFileW(
            tmp_w.as_ptr(),
            GENERIC_WRITE,
            0,
            core::ptr::null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return false; // :2608
    }
    // SAFETY: `handle` is the live handle just opened; ownership moves to the `File`, which
    // closes it exactly once (replacing the C's `CloseHandle`).
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    let mut done = 0usize;
    let mut ok = true;
    while done < data.len() {
        let chunk = (data.len() - done).min(0x400_0000); // :2614
        match file.write(&data[done..done + chunk]) {
            Ok(0) | Err(_) => {
                ok = false; // :2616
                break;
            }
            Ok(w) => done += w,
        }
    }
    drop(file); // CloseHandle (`:2619`)
    if !ok {
        // SAFETY: `tmp_w` is the live path the failed write created.
        unsafe { DeleteFileW(tmp_w.as_ptr()) };
        return false;
    }
    // SAFETY: both paths are NUL-terminated and outlive the call; REPLACE_EXISTING is the C's
    // flag.
    let moved = unsafe { MoveFileExW(tmp_w.as_ptr(), path_w.as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if moved == 0 {
        // SAFETY: as above.
        unsafe { DeleteFileW(tmp_w.as_ptr()) };
        return false; // :2621
    }
    true
}

/// The absent-platform `write_file_atomic`: no tools filesystem exists, and every render body is
/// behind the `-32017` gate. Answering `false` gives the C's "could not write" error if a body is
/// ever driven directly.
#[cfg(not(windows))]
pub(crate) fn write_file_atomic(_path: &[u8], _data: &[u8]) -> bool {
    false
}

/// What `/name` selects (`mcp_bridge.c:2626-2645`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum NameChoice {
    /// No usable `name` member, or an empty one: the C falls through to the default branch.
    Default,
    /// A `name` that passed the rule (`:2632`), a trailing `.png` already dropped.
    Explicit(Vec<u8>),
    /// A `name` that fails the rule (`:2630`): the C answers `0` without touching the counter.
    Refused,
}

/// The `name` member's rule only (`mcp_bridge.c:2626-2633`): `[A-Za-z0-9._-]`, 1..64 chars, no
/// leading dot, with a trailing `.png` dropped.
fn name_choice(params: Option<&str>) -> NameChoice {
    let Some(params) = params else {
        return NameChoice::Default;
    };
    let mut raw = [0u8; 96];
    if !json_string(params.as_bytes(), b"name", &mut raw) || raw[0] == 0 {
        return NameChoice::Default; // :2626
    }
    let mut n = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if n > 4
        && raw[n - 4] == b'.'
        && (raw[n - 3] | 32) == b'p'
        && (raw[n - 2] | 32) == b'n'
        && (raw[n - 1] | 32) == b'g'
    {
        raw[n - 4] = 0;
        n -= 4;
    }
    if raw[0] == b'.' || n == 0 || n > 64 {
        return NameChoice::Refused; // :2630
    }
    for &c in &raw[..n] {
        let ok = c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-';
        if !ok {
            return NameChoice::Refused;
        }
    }
    NameChoice::Explicit(raw[..n].to_vec())
}

/// The C's default branch (`mcp_bridge.c:2639-2645`): `render_` + tick + `_` + seq in a `cap`-byte
/// SB whose overflow the C ignores.
fn default_stem(tick: u64, seq: u64, cap: usize) -> Vec<u8> {
    let mut stem = Vec::new();
    stem.extend_from_slice(b"render_");
    stem.extend_from_slice(tick.to_string().as_bytes());
    stem.push(b'_');
    stem.extend_from_slice(seq.to_string().as_bytes());
    stem.truncate(cap.saturating_sub(1));
    stem
}

/// `render_name` (`mcp_bridge.c:2624-2646`): the file stem. `None` is the C's `0` (a `name` that
/// fails the character rule).
///
/// The tick and `++g_renderSeq` are read **inside the default branch only**, exactly as the C's
/// `render_name` does after all earlier validation: an explicit `name` and a refused one leave
/// `g_renderSeq` untouched (`:2643`).
pub(crate) fn render_name(params: Option<&str>, cap: usize) -> Option<Vec<u8>> {
    render_name_with(params, cap, name_tick, next_seq)
}

/// [`render_name`] over the two producers, so the host tests can prove the C's laziness without
/// the process-wide counter.
fn render_name_with(
    params: Option<&str>,
    cap: usize,
    tick: impl FnOnce() -> u64,
    seq: impl FnOnce() -> u64,
) -> Option<Vec<u8>> {
    match name_choice(params) {
        NameChoice::Refused => None, // :2630
        NameChoice::Explicit(mut stem) => {
            stem.truncate(cap.saturating_sub(1)); // cpy(stem, raw, cap)
            Some(stem)
        }
        NameChoice::Default => Some(default_stem(tick(), seq(), cap)),
    }
}

/// `sb_addf3` (`mcp_bridge.c:2648-2655`): `[x,y,z]` with the C's float format.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn sb_addf3(b: &mut Sb, v: [f32; 3]) {
    b.add(b"[");
    b.addf(v[0]);
    b.add(b",");
    b.addf(v[1]);
    b.add(b",");
    b.addf(v[2]);
    b.add(b"]");
}

/// `frustum_set` (`mcp_bridge.c:2657-2666`): the `CFrustum` fields Hammer's screenshot dialog
/// sets after the default init. The C's bridge-owned `CFrustum` is the generated layout (the
/// `_Static_assert`s in `bridge/s2sdk.h` bind `fovX`/`aspect`/`znear`/`ndc` to
/// `S2_OFF_Camera_t_*` / `S2_OFF_CFrustum_m_flClipSpace*`), so the writes go through the
/// generated fields; `reserved28` is `Camera_t::m_flWidth`/`m_flHeight`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn frustum_set(
    f: &mut s2sdk_sys::generated::CFrustum,
    origin: [f32; 3],
    angles: [f32; 3],
    fov: f32,
    aspect: f32,
    znear: f32,
    zfar: f32,
) {
    f.m_camera.m_origin = s2sdk_sys::generated::Vector {
        x: origin[0],
        y: origin[1],
        z: origin[2],
    };
    f.m_camera.m_angles = s2sdk_sys::generated::QAngle {
        x: angles[0],
        y: angles[1],
        z: angles[2],
    };
    f.m_camera.m_flFOVX = fov;
    f.m_camera.m_flAspect = aspect;
    f.m_camera.m_flZNear = znear;
    f.m_camera.m_flZFar = zfar;
    f.m_camera.m_flWidth = -1.0; // reserved28[0]
    f.m_camera.m_flHeight = -1.0; // reserved28[1]
    f.m_flClipSpaceBottomLeftX = -1.0; // ndc[0]
    f.m_flClipSpaceBottomLeftY = -1.0; // ndc[1]
    f.m_flClipSpaceTopRightX = 1.0; // ndc[2]
    f.m_flClipSpaceTopRightY = 1.0; // ndc[3]
}

// ---------------------------------------------------------------------------------------------
// Convert + stats (`mcp_bridge.c:2767-2791`, `:3036-3054`, `pet_render.inc:98-120`)
// ---------------------------------------------------------------------------------------------

/// The stats block's inputs, filled by [`convert_view`] / [`convert_world`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    /// `untouched`: pixels still equal to [`VR_SENTINEL`].
    pub untouched: u64,
    /// `lumaSum`.
    pub luma_sum: u64,
    /// `nonBlack`.
    pub non_black: u64,
    /// `lumaMin`, seeded at 255 as the C does.
    pub luma_min: u32,
    /// `lumaMax`, seeded at 0.
    pub luma_max: u32,
}

/// `(54*r + 183*g + 19*bl) >> 8` (`:2785`).
fn luma(r: u8, g: u8, bl: u8) -> u32 {
    (54 * u32::from(r) + 183 * u32::from(g) + 19 * u32::from(bl)) >> 8
}

/// The stats writer shared by the three reply shapes (`:2845-2852`, `:3092-3099`,
/// `pet_render.inc:156-163`): `,"stats":{...}`, with the CRC as eight lowercase hex digits.
pub(crate) fn add_stats(b: &mut Sb, stats: &Stats, npx: u64, crc: u32) {
    b.add(b",\"stats\":{\"untouchedPixels\":");
    b.addnum(stats.untouched);
    b.add(b",\"nonBlackPixels\":");
    b.addnum(stats.non_black);
    b.add(b",\"meanLuma\":");
    b.addf((stats.luma_sum as f64 / npx as f64) as f32);
    b.add(b",\"minLuma\":");
    b.addnum(u64::from(stats.luma_min));
    b.add(b",\"maxLuma\":");
    b.addnum(u64::from(stats.luma_max));
    b.add(b",\"crc32\":\"");
    for i in (0..8).rev() {
        let nibble = (crc >> (4 * i)) & 15;
        b.putc(b"0123456789abcdef"[nibble as usize]);
    }
    b.add(b"\"}");
}

/// The `view.render` convert loop (`mcp_bridge.c:2767-2791`): `bpp` bytes per pixel in `px`
/// (`3` for RGB888, `4` for RGBA8888), optional sRGB LUT, the C's per-byte sentinel test.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn convert_view(
    px: &[u8],
    w: u32,
    h: u32,
    bpp: u32,
    srgb: bool,
) -> (Vec<u8>, Stats) {
    let stride = 1 + 3 * w as usize;
    let mut raw = vec![0u8; stride * h as usize];
    let mut stats = Stats {
        luma_min: 255,
        ..Stats::default()
    };
    for y in 0..h {
        let row = y as usize * stride;
        raw[row] = 0;
        for x in 0..w {
            let bi = (y as u64 * u64::from(w) + u64::from(x)) * u64::from(bpp);
            let s = &px[bi as usize..bi as usize + bpp as usize];
            let mut sent = true;
            for k in 0..bpp {
                let gi = bi + u64::from(k);
                if s[k as usize] != (VR_SENTINEL >> (8 * (gi & 3))) as u8 {
                    sent = false;
                    break;
                }
            }
            stats.untouched += u64::from(sent);
            let mut r = s[0];
            let mut g = s[1];
            let mut bl = s[2];
            if srgb {
                r = SRGB_LUT[usize::from(r)];
                g = SRGB_LUT[usize::from(g)];
                bl = SRGB_LUT[usize::from(bl)];
            }
            let at = row + 1 + 3 * x as usize;
            raw[at] = r;
            raw[at + 1] = g;
            raw[at + 2] = bl;
            let l = luma(r, g, bl);
            stats.luma_sum += u64::from(l);
            stats.luma_min = stats.luma_min.min(l);
            stats.luma_max = stats.luma_max.max(l);
            if l > 8 {
                stats.non_black += 1;
            }
        }
    }
    (raw, stats)
}

/// The `world.render` / ToolScene convert loop (`mcp_bridge.c:3036-3054`,
/// `pet_render.inc:98-120`): 4 bytes per pixel, the sentinel as one `u32` word, optional sRGB
/// LUT (world only; the ToolScene pixels are display-ready).
pub(crate) fn convert_word_pixels(
    px: &[u8],
    w: u32,
    h: u32,
    srgb: bool,
) -> (Vec<u8>, Stats) {
    let stride = 1 + 3 * w as usize;
    let mut raw = vec![0u8; stride * h as usize];
    let mut stats = Stats {
        luma_min: 255,
        ..Stats::default()
    };
    for y in 0..h {
        let row = y as usize * stride;
        raw[row] = 0;
        for x in 0..w {
            let i = y as u64 * u64::from(w) + u64::from(x);
            let at4 = i as usize * 4;
            let word = u32::from_le_bytes([px[at4], px[at4 + 1], px[at4 + 2], px[at4 + 3]]);
            if word == VR_SENTINEL {
                stats.untouched += 1;
            }
            let (mut r, mut g, mut bl) = (px[at4], px[at4 + 1], px[at4 + 2]);
            if srgb {
                r = SRGB_LUT[usize::from(r)];
                g = SRGB_LUT[usize::from(g)];
                bl = SRGB_LUT[usize::from(bl)];
            }
            let at = row + 1 + 3 * x as usize;
            raw[at] = r;
            raw[at + 1] = g;
            raw[at + 2] = bl;
            let l = luma(r, g, bl);
            stats.luma_sum += u64::from(l);
            stats.luma_min = stats.luma_min.min(l);
            stats.luma_max = stats.luma_max.max(l);
            if l > 8 {
                stats.non_black += 1;
            }
        }
    }
    (raw, stats)
}

/// Fill `px` with the sentinel the way `view.render` does (`:2749-2752`): a `u32` written four
/// bytes at a time, clipped at the end of the buffer.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn fill_sentinel_strided(px: &mut [u8]) {
    let mut i = 0;
    while i < px.len() {
        for k in 0..4 {
            if i + k < px.len() {
                px[i + k] = (VR_SENTINEL >> (8 * k)) as u8;
            }
        }
        i += 4;
    }
}

/// Fill `px` with the sentinel the way `world.render` / the ToolScene tail do (`:2959`,
/// `pet_render.inc:90`): one `u32` per pixel.
pub(crate) fn fill_sentinel_u32(px: &mut [u8]) {
    let (words, rest) = px.as_chunks_mut::<4>();
    for word in words {
        word.copy_from_slice(&VR_SENTINEL.to_le_bytes());
    }
    let _ = rest;
}

// ---------------------------------------------------------------------------------------------
// Parameter contracts
// ---------------------------------------------------------------------------------------------

/// `-32602 InvalidParams` with the C's `reason` (`op_err`).
pub(crate) fn invalid(reason: &str) -> OpErr {
    OpErr::new(-32602, "InvalidParams", reason)
}

/// The live camera's values `view.render` starts from (`mcp_bridge.c:2690-2700`).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct CameraFacts {
    /// `CameraGetPosition`.
    pub origin: [f32; 3],
    /// `CameraGetAngles`.
    pub angles: [f32; 3],
    /// `live->fovX`.
    pub fov: f32,
    /// `live->aspect`.
    pub aspect: f32,
    /// `live->znear`.
    pub znear: f32,
    /// `live->zfar`.
    pub zfar: f32,
}

/// `view.render`'s parsed parameters and derived camera (`mcp_bridge.c:2680-2731`).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct ViewParams {
    pub w: u32,
    pub h: u32,
    pub format: i64,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub fov: f32,
    pub aspect: f32,
    pub znear: f32,
    pub zfar: f32,
    pub srgb: bool,
    pub hide_tools: bool,
    pub source: &'static str,
    pub stem: Vec<u8>,
}

/// The width/height/format part of `op_view_render`'s parameters (`mcp_bridge.c:2680-2688`).
///
/// The C checks these **before** it reads the camera (`:2690`), so the first game access in
/// `view.render` happens after this refusal; the rest of the parameter block is
/// [`view_viewport`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct ViewShape {
    pub w: u32,
    pub h: u32,
    pub format: i64,
}

/// The C's size/format checks (`mcp_bridge.c:2681-2688`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn view_shape(params: Option<&str>) -> Result<ViewShape, OpErr> {
    let mut lw = 640i64;
    let mut lh = 360i64;
    let mut lfmt = 0i64;
    if let Some(p) = params {
        if let Some(v) = json_int(p.as_bytes(), b"width") {
            lw = v; // :2682
        }
        if let Some(v) = json_int(p.as_bytes(), b"height") {
            lh = v;
        }
        if let Some(v) = json_int(p.as_bytes(), b"format") {
            lfmt = v;
        }
    }
    if lw < 16
        || lh < 16
        || lw > VR_MAX_SIDE
        || lh > VR_MAX_SIDE
        || (lw as u64) * (lh as u64) > VR_MAX_PIXELS
    {
        return Err(invalid(
            "width/height: 16..4096 each, at most 8388608 pixels", // :2684
        ));
    }
    if lfmt != 0 && lfmt != 2 {
        return Err(invalid("format: 0 (RGBA8888) or 2 (RGB888)")); // :2687
    }
    Ok(ViewShape {
        w: lw as u32,
        h: lh as u32,
        format: lfmt,
    })
}

/// `view.render`'s camera fields (`mcp_bridge.c:2690-2731`), in the C's order: the live camera is
/// an input because the C read it after the size checks and before parsing the camera members.
/// `fov` defaults to the viewport's own when it is in `(1, 179)`, `znear`/`zfar` to `1`/`9000`
/// unless the viewport has its own pair, and `origin`/`angles` to the camera's.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn view_viewport(
    params: Option<&str>,
    live: &CameraFacts,
    shape: ViewShape,
) -> Result<ViewParams, OpErr> {
    let w = shape.w;
    let h = shape.h;
    let lfmt = shape.format;

    let mut origin = live.origin;
    let mut angles = live.angles;
    let mut fov = if live.fov > 1.0 && live.fov < 179.0 {
        live.fov
    } else {
        90.0
    }; // :2698
    let mut znear = 1.0f32;
    let mut zfar = 9000.0f32;
    if live.znear > 0.0 && live.zfar > live.znear && live.zfar <= 1_000_000.0 {
        znear = live.znear;
        zfar = live.zfar;
    } // :2700

    let mut has_origin = false;
    let mut has_angles = false;
    if let Some(p) = params {
        if json_find(p.as_bytes(), b"origin").is_some() {
            origin = json_float3(p.as_bytes(), b"origin")
                .ok_or_else(|| invalid("origin: [x,y,z]"))?; // :2704-2706
            has_origin = true;
        }
        if json_find(p.as_bytes(), b"angles").is_some() {
            angles = json_float3(p.as_bytes(), b"angles")
                .ok_or_else(|| invalid("angles: [pitch,yaw,roll]"))?; // :2708-2710
            has_angles = true;
        }
        if let Some(v) = json_float1(p.as_bytes(), b"fov") {
            fov = v; // :2713
        }
        if let Some(v) = json_float1(p.as_bytes(), b"znear") {
            znear = v;
        }
        if let Some(v) = json_float1(p.as_bytes(), b"zfar") {
            zfar = v;
        }
    }
    if !(fov > 1.0 && fov < 179.0) {
        return Err(invalid("fov: (1, 179) degrees")); // :2717
    }
    if !(znear > 0.0 && zfar > znear && zfar <= 1_000_000.0) {
        return Err(invalid("znear/zfar")); // :2718
    }
    let mut srgb = lfmt == 0; // :2719
    if let Some(p) = params {
        if let Some(v) = json_bool(p.as_bytes(), b"srgb") {
            srgb = v; // :2720
        }
    }
    let mut hide_tools = false;
    if let Some(p) = params {
        if let Some(v) = json_bool(p.as_bytes(), b"hideToolMaterials") {
            hide_tools = v; // :2725
        }
    }
    let aspect = w as f32 / h as f32; // :2726
    let stem = render_name(params, 80)
        .ok_or_else(|| invalid("name: [A-Za-z0-9._-], 1..64 chars, no leading dot"))?; // :2728-2730
    let source = match (has_origin, has_angles) {
        (true, true) => "explicit",
        (true, false) | (false, true) => "mixed",
        (false, false) => "viewport",
    }; // :2830
    Ok(ViewParams {
        w,
        h,
        format: lfmt,
        origin,
        angles,
        fov,
        aspect,
        znear,
        zfar,
        srgb,
        hide_tools,
        source,
        stem,
    })
}

/// `world.render`'s parsed parameters (`mcp_bridge.c:2929-2948`).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct WorldParams {
    pub w: u32,
    pub h: u32,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub fov: f32,
    pub znear: f32,
    pub zfar: f32,
    pub srgb: bool,
    pub stem: Vec<u8>,
}

/// `origin` + `angles` are required: there is no viewport camera to default to.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn world_params(params: Option<&str>) -> Result<WorldParams, OpErr> {
    let mut lw = 1920i64;
    let mut lh = 1080i64;
    let mut fov = 90.0f32;
    let mut znear = 4.0f32;
    let mut zfar = 30000.0f32;
    let Some(p) = params else {
        return Err(invalid("origin: [x,y,z] required")); // :2932
    };
    let origin = json_float3(p.as_bytes(), b"origin")
        .ok_or_else(|| invalid("origin: [x,y,z] required"))?;
    let angles = json_float3(p.as_bytes(), b"angles")
        .ok_or_else(|| invalid("angles: [pitch,yaw,roll] required"))?; // :2933
    if let Some(v) = json_int(p.as_bytes(), b"width") {
        lw = v; // :2934
    }
    if let Some(v) = json_int(p.as_bytes(), b"height") {
        lh = v;
    }
    if let Some(v) = json_float1(p.as_bytes(), b"fov") {
        fov = v; // :2935
    }
    if let Some(v) = json_float1(p.as_bytes(), b"znear") {
        znear = v;
    }
    if let Some(v) = json_float1(p.as_bytes(), b"zfar") {
        zfar = v;
    }
    if lw < 16
        || lh < 16
        || lw > WR_MAX_SIDE
        || lh > WR_MAX_SIDE
        || (lw as u64) * (lh as u64) > WR_MAX_PIXELS
    {
        return Err(invalid(
            "width/height: 16..8192 each, at most 33554432 pixels", // :2937
        ));
    }
    if !(fov > 1.0 && fov < 179.0) {
        return Err(invalid("fov: (1, 179) degrees, horizontal")); // :2940
    }
    if !(znear > 0.0 && zfar > znear && zfar <= 1_000_000.0) {
        return Err(invalid("znear/zfar")); // :2941
    }
    let mut srgb = false; // :2942
    if let Some(v) = json_bool(p.as_bytes(), b"srgb") {
        srgb = v; // :2943
    }
    let stem = render_name(params, 80)
        .ok_or_else(|| invalid("name: [A-Za-z0-9._-], 1..64 chars, no leading dot"))?; // :2945-2947
    Ok(WorldParams {
        w: lw as u32,
        h: lh as u32,
        origin,
        angles,
        fov,
        znear,
        zfar,
        srgb,
        stem,
    })
}

/// `toolscene_render_params` (`mcp_bridge_pet_render.inc:57-72`): the `pet.render` /
/// `modeldoc.render` contract - validated before any object is touched.
pub(crate) fn toolscene_params(
    params: Option<&str>,
) -> Result<(u32, u32, Vec<u8>), OpErr> {
    let mut lw = 1024i64;
    let mut lh = 768i64;
    if let Some(p) = params {
        if let Some(v) = json_int(p.as_bytes(), b"width") {
            lw = v; // :60
        }
        if let Some(v) = json_int(p.as_bytes(), b"height") {
            lh = v;
        }
    }
    if lw < 16
        || lh < 16
        || lw > VR_MAX_SIDE
        || lh > VR_MAX_SIDE
        || (lw as u64) * (lh as u64) > VR_MAX_PIXELS
    {
        return Err(invalid(
            "width/height: 16..4096 each, at most 8388608 pixels", // :62
        ));
    }
    let stem = render_name(params, 80)
        .ok_or_else(|| invalid("name: [A-Za-z0-9._-], 1..64 chars, no leading dot"))?; // :65-67
    Ok((lw as u32, lh as u32, stem))
}

// ---------------------------------------------------------------------------------------------
// The arm tails
// ---------------------------------------------------------------------------------------------

/// The `world.render` / `view.render` arm tail (`mcp_bridge.c:6051-6077`): error with `data`,
/// bare error, else the result envelope. The C arms have **no** `rb.ovf` branch - a builder that
/// ran out of its 4096 bytes is appended truncated - so neither does this one.
pub(crate) fn finish_render(id: &str, outcome: Result<(), OpErr>, result: &Sb) -> String {
    match outcome {
        Err(e) if e.data.is_empty() => crate::control::error(Some(id), e.code, e.name),
        Err(e) => crate::control::error_ext(Some(id), e.code, e.name, &e.data),
        Ok(()) => crate::control::result_ok(Some(id), &String::from_utf8_lossy(result.as_cstr())),
    }
}

/// The C's `pet.*` arm tail (`mcp_bridge.c:6222-6231`) over an [`Sb`]: the same four arms as
/// [`crate::ops::pet::finish_pet`] (which is the same tail over the pet lane's `Buf`), including
/// the `-32603 InternalError: pet result too large` overflow answer. `pet.render` writes into an
/// `Sb` because its reply carries `meanLuma`, which `Buf` does not build; the host tests hold the
/// two spellings to the same bytes.
pub(crate) fn finish_pet_sb(id: &str, outcome: Result<(), OpErr>, result: &Sb) -> String {
    match outcome {
        Err(e) if e.data.is_empty() => crate::control::error(Some(id), e.code, e.name),
        Err(e) => crate::control::error_ext(Some(id), e.code, e.name, &e.data),
        Ok(()) if result.ovf() => crate::control::error(
            Some(id),
            -32603,
            "InternalError: pet result too large",
        ),
        Ok(()) => crate::control::result_ok(Some(id), &String::from_utf8_lossy(result.as_cstr())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A builder over a fresh buffer, the way every op holds one.
    fn sb(buf: &mut [u8]) -> Sb<'_> {
        Sb::new(buf)
    }

    // -----------------------------------------------------------------------------------------
    // PNG (`mcp_bridge.c:2533-2601`)
    // -----------------------------------------------------------------------------------------

    /// The standard CRC-32/A test vector: `crc_init`/`crc_update` are the C's table and
    /// complement, so a different table would answer differently.
    #[test]
    fn crc32_is_the_c_algorithm() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // the published CRC-32/ISO-HDLC vector
        assert_eq!(crc32(b""), 0);
    }

    /// A 2x2 RGB8 image: signature, IHDR (13 bytes, 8-bit RGB), one stored-block IDAT,
    /// IEND. The `png` dev-dependency decodes it and verifies every chunk CRC; the pixel
    /// bytes must come back row by row.
    #[test]
    fn png_encode_is_a_decodable_rgb8_png() {
        let (w, h) = (2u32, 2u32);
        // rows of {filter 0, r,g,b, r,g,b}
        let raw = vec![
            0, 255, 0, 0, 0, 255, 0, // red, green
            0, 0, 0, 255, 9, 9, 9, // blue, grey
        ];
        let png = png_encode(&raw, w, h).expect("no zlen refusal");
        let raw_len = u64::from(h) * (1 + 3 * u64::from(w));
        let nblk = raw_len.div_ceil(65535);
        let zlen = 2 + raw_len + 5 * nblk + 4;
        assert_eq!(png.len() as u64, 8 + 25 + (12 + zlen) + 12, "the C's total");

        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("IHDR parses");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("a bounded image")];
        let info = reader.next_frame(&mut buf).expect("IDAT decodes");
        assert_eq!((info.width, info.height), (w, h));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        assert_eq!(&buf[..info.buffer_size()], &[255, 0, 0, 0, 255, 0, 0, 0, 255, 9, 9, 9]);
    }

    /// The stored-deflate block machinery across a 65535-byte boundary: three rows of a
    /// 30000x1 image are 90003 bytes, so the C emits two blocks and sets BFINAL only on the
    /// last.
    #[test]
    fn png_encode_spans_stored_blocks() {
        let (w, h) = (30000u32, 1u32);
        let mut raw = vec![7u8; (1 + 3 * 30000) as usize];
        raw[0] = 0; // the filter byte of the only row
        let png = png_encode(&raw, w, h).expect("no zlen refusal");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("IHDR parses");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("a bounded image")];
        let info = reader.next_frame(&mut buf).expect("IDAT decodes");
        assert_eq!(&buf[..info.buffer_size()], &raw[1..]); // the decoder drops the filter byte
    }

    // -----------------------------------------------------------------------------------------
    // The name rule (`mcp_bridge.c:2624-2646`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn render_name_keeps_the_c_rules() {
        let name = |params: Option<&str>| render_name_with(params, 80, || 77, || 3);
        // no name / empty name -> render_<tick>_<seq>
        assert_eq!(name(None).unwrap(), b"render_77_3");
        assert_eq!(name(Some("{}")).unwrap(), b"render_77_3");
        assert_eq!(name(Some(r#"{"name":""}"#)).unwrap(), b"render_77_3");
        // a usable name, and a trailing .png dropped case-insensitively
        assert_eq!(name(Some(r#"{"name":"shot"}"#)).unwrap(), b"shot");
        assert_eq!(name(Some(r#"{"name":"shot.png"}"#)).unwrap(), b"shot");
        assert_eq!(name(Some(r#"{"name":"shot.PNG"}"#)).unwrap(), b"shot");
        assert_eq!(name(Some(r#"{"name":"a.b_c-d9"}"#)).unwrap(), b"a.b_c-d9");
        // refusals: leading dot, bad character, more than 64 bytes
        assert_eq!(name(Some(r#"{"name":".x"}"#)), None); // :2630
        assert_eq!(name(Some(r#"{"name":"../x"}"#)), None);
        assert_eq!(name(Some(r#"{"name":"a b"}"#)), None);
        let long = "a".repeat(65);
        assert_eq!(name(Some(&format!(r#"{{"name":"{long}"}}"#))), None); // :2630
        let edge = "a".repeat(64);
        assert_eq!(
            name(Some(&format!(r#"{{"name":"{edge}"}}"#))).unwrap(),
            edge.as_bytes()
        );
    }

    /// `++g_renderSeq` (`mcp_bridge.c:2643`) runs **only** in `render_name`'s default branch,
    /// after all earlier validation: a refused name (`:2630`) and an explicit one (`:2632`) leave
    /// the counter and the tick untouched, so the next default name is `render_<tick>_1`.
    #[test]
    fn the_default_name_counter_moves_only_in_the_default_branch() {
        let seq_calls = core::cell::Cell::new(0u64);
        let tick_calls = core::cell::Cell::new(0u64);
        let seq = || {
            seq_calls.set(seq_calls.get() + 1);
            seq_calls.get()
        };
        let tick = || {
            tick_calls.set(tick_calls.get() + 1);
            500u64
        };

        // An explicit `name` is returned as-is: no tick read, no counter bump.
        assert_eq!(
            render_name_with(Some(r#"{"name":"shot.png"}"#), 80, tick, seq).unwrap(),
            b"shot"
        );
        assert_eq!((seq_calls.get(), tick_calls.get()), (0, 0));
        // A refused `name` answers the C's 0, and also bumps nothing.
        assert_eq!(
            render_name_with(Some(r#"{"name":".x"}"#), 80, tick, seq),
            None
        );
        assert_eq!((seq_calls.get(), tick_calls.get()), (0, 0));
        // The default branch reads both once, and the first default name ends in `_1`.
        assert_eq!(
            render_name_with(None, 80, tick, seq).unwrap(),
            b"render_500_1"
        );
        assert_eq!((seq_calls.get(), tick_calls.get()), (1, 1));
    }

    // -----------------------------------------------------------------------------------------
    // json_bool (`mcp_bridge.c:2525-2531`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn json_bool_reads_the_c_literals_only() {
        assert_eq!(json_bool(br#"{"a":true}"#, b"a"), Some(true));
        assert_eq!(json_bool(br#"{"a":false}"#, b"a"), Some(false));
        assert_eq!(json_bool(br#"{"a":0}"#, b"a"), None);
        assert_eq!(json_bool(br#"{"b":true}"#, b"a"), None);
        assert_eq!(json_bool(br#"{"a":tru}"#, b"a"), None);
    }

    // -----------------------------------------------------------------------------------------
    // sb_addf3 (`mcp_bridge.c:2648-2655`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn sb_addf3_writes_the_c_triple() {
        let mut buf = vec![0u8; 64];
        let mut b = sb(&mut buf);
        sb_addf3(&mut b, [1.0, -2.5, 0.125]);
        assert_eq!(b.as_cstr(), b"[1,-2.500,0.125]");
    }

    // -----------------------------------------------------------------------------------------
    // frustum_set (`mcp_bridge.c:2657-2666`)
    // -----------------------------------------------------------------------------------------

    /// The C's bridge-owned `CFrustum` field offsets are the generated ones (the `_Static_assert`s
    /// in `bridge/s2sdk.h:283-292` bind them), so `frustum_set` writes the generated fields.
    #[test]
    fn frustum_set_writes_the_c_fields() {
        use core::mem::offset_of;
        use s2sdk_sys::generated::{Camera_t, CFrustum};
        assert_eq!(offset_of!(Camera_t, m_flFOVX), 0x18);
        assert_eq!(offset_of!(Camera_t, m_flWidth), 0x28); // reserved28[0]
        assert_eq!(offset_of!(Camera_t, m_flHeight), 0x2C); // reserved28[1]
        assert_eq!(offset_of!(CFrustum, m_flClipSpaceBottomLeftX), 0x170); // ndc[0]
        assert_eq!(core::mem::size_of::<CFrustum>(), 1056); // S2_SIZEOF_CFrustum == 0x420

        // SAFETY: a zeroed local of the plain-C struct; every byte is initialised.
        let mut f: CFrustum = unsafe { core::mem::zeroed() };
        frustum_set(&mut f, [1.0, 2.0, 3.0], [4.0, 5.0, 6.0], 95.0, 1.5, 2.0, 500.0);
        assert_eq!((f.m_camera.m_origin.x, f.m_camera.m_origin.y, f.m_camera.m_origin.z), (1.0, 2.0, 3.0));
        assert_eq!((f.m_camera.m_angles.x, f.m_camera.m_angles.y, f.m_camera.m_angles.z), (4.0, 5.0, 6.0));
        assert_eq!(f.m_camera.m_flFOVX, 95.0);
        assert_eq!(f.m_camera.m_flAspect, 1.5);
        assert_eq!((f.m_camera.m_flZNear, f.m_camera.m_flZFar), (2.0, 500.0));
        assert_eq!((f.m_camera.m_flWidth, f.m_camera.m_flHeight), (-1.0, -1.0));
        assert_eq!(
            (
                f.m_flClipSpaceBottomLeftX,
                f.m_flClipSpaceBottomLeftY,
                f.m_flClipSpaceTopRightX,
                f.m_flClipSpaceTopRightY
            ),
            (-1.0, -1.0, 1.0, 1.0)
        );
    }

    // -----------------------------------------------------------------------------------------
    // Convert + stats (`mcp_bridge.c:2767-2791`, `:3036-3054`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn convert_view_keeps_the_c_pixel_maths() {
        // One 2x1 RGBA row: a black pixel and a sentinel-shaped one (the C's strided fill
        // writes A55A5AA5 starting at byte 4, so pixel 1 is A5,5A,5A,A5).
        let px = vec![0u8, 0, 0, 255, 0xA5, 0x5A, 0x5A, 0xA5];
        let (raw, stats) = convert_view(&px, 2, 1, 4, false);
        assert_eq!(raw, vec![0u8, 0, 0, 0, 165, 90, 90]);
        assert_eq!(stats.untouched, 1);
        assert_eq!(stats.luma_min, 0);
        assert_eq!(stats.luma_max, (54 * 165u32 + 183 * 90 + 19 * 90) >> 8);
        // 54*0 = 0 for the first pixel; meanLuma is the sum over both.
        assert_eq!(stats.luma_sum, u64::from(stats.luma_max));
        assert_eq!(stats.non_black, 1);

        // The sRGB LUT applies per channel when asked (view.render's format 0 default).
        let px = vec![255u8, 128, 0, 255];
        let (raw, _) = convert_view(&px, 1, 1, 4, true);
        assert_eq!(raw, vec![0u8, 254, 187, 0]); // k_srgbLut[255], [128], [0]
    }

    #[test]
    fn sentinel_fill_and_word_conversion_match_the_c() {
        // view.render: a u32 written four bytes at a time, clipped at the end (`:2749-2752`).
        let mut px = vec![0u8; 6];
        fill_sentinel_strided(&mut px);
        assert_eq!(px, vec![0xA5, 0x5A, 0x5A, 0xA5, 0xA5, 0x5A]);

        // world.render / the ToolScene tail: one u32 per pixel (`:2959`, `pet_render.inc:90`).
        let mut px = vec![0u8; 8];
        fill_sentinel_u32(&mut px);
        assert_eq!(px, vec![0xA5, 0x5A, 0x5A, 0xA5, 0xA5, 0x5A, 0x5A, 0xA5]);
        let (raw, stats) = convert_word_pixels(&px, 2, 1, false);
        assert_eq!(stats.untouched, 2);
        assert_eq!(raw.len(), 7);
    }

    // -----------------------------------------------------------------------------------------
    // view.render's parameters (`mcp_bridge.c:2680-2731`)
    // -----------------------------------------------------------------------------------------

    fn live_camera() -> CameraFacts {
        CameraFacts {
            origin: [10.0, 20.0, 30.0],
            angles: [40.0, 50.0, 0.0],
            fov: 73.0,
            aspect: 1.5,
            znear: 2.0,
            zfar: 5000.0,
        }
    }

    /// The two phases the live body runs: [`view_shape`] first (`:2680-2688`), then the camera
    /// fields after the camera read ([`view_viewport`]).
    fn shape() -> ViewShape {
        view_shape(None).expect("the defaults")
    }

    #[test]
    fn view_shape_defaults_and_bounds() {
        let s = view_shape(None).expect("the defaults");
        assert_eq!((s.w, s.h, s.format), (640, 360, 0)); // :2681
        let s = view_shape(Some(r#"{"width":320,"height":180,"format":2}"#)).expect("valid");
        assert_eq!((s.w, s.h, s.format), (320, 180, 2));
        let err = |params: &str| view_shape(Some(params)).unwrap_err();
        let e = err(r#"{"width":8}"#);
        assert_eq!((e.code, e.name), (-32602, "InvalidParams"));
        assert_eq!(e.data, r#"{"reason":"width/height: 16..4096 each, at most 8388608 pixels"}"#); // :2684
        assert_eq!(
            err(r#"{"width":4096,"height":4096}"#).data,
            r#"{"reason":"width/height: 16..4096 each, at most 8388608 pixels"}"#
        );
        assert_eq!(
            err(r#"{"format":1}"#).data,
            r#"{"reason":"format: 0 (RGBA8888) or 2 (RGB888)"}"# // :2687
        );
    }

    #[test]
    fn view_viewport_defaults_come_from_the_viewport() {
        let p = view_viewport(None, &live_camera(), shape()).expect("defaults");
        assert_eq!((p.w, p.h), (640, 360));
        assert_eq!(p.format, 0);
        assert_eq!(p.origin, [10.0, 20.0, 30.0]); // :2696
        assert_eq!(p.angles, [40.0, 50.0, 0.0]); // :2697
        assert_eq!(p.fov, 73.0); // :2698
        assert_eq!((p.znear, p.zfar), (2.0, 5000.0)); // :2700
        assert_eq!(p.source, "viewport"); // :2830
        assert!(p.srgb); // format 0 (`:2719`)
        assert!(!p.hide_tools);
        assert_eq!(p.aspect, 640.0 / 360.0); // :2726
        // The default stem is `render_<GetTickCount64>_<seq>`; its bytes are pinned by
        // `the_default_name_counter_moves_only_in_the_default_branch`.
        assert!(p.stem.starts_with(b"render_"), "{:?}", p.stem);
    }

    #[test]
    fn view_viewport_defaults_to_the_thumbnail_values_without_a_usable_live_camera() {
        let odd = CameraFacts {
            fov: 0.5,
            znear: 0.0,
            zfar: 10_000_000.0,
            ..live_camera()
        };
        let p = view_viewport(None, &odd, shape()).expect("defaults");
        assert_eq!(p.fov, 90.0); // :2698
        assert_eq!((p.znear, p.zfar), (1.0, 9000.0)); // :2699
    }

    #[test]
    fn view_viewport_source_is_explicit_mixed_or_viewport() {
        let explicit = view_viewport(
            Some(r#"{"origin":[1,2,3],"angles":[4,5,6]}"#),
            &live_camera(),
            shape(),
        )
        .expect("explicit");
        assert_eq!(explicit.origin, [1.0, 2.0, 3.0]);
        assert_eq!(explicit.angles, [4.0, 5.0, 6.0]);
        assert_eq!(explicit.source, "explicit");
        let mixed = view_viewport(Some(r#"{"origin":[1,2,3]}"#), &live_camera(), shape())
            .expect("mixed");
        assert_eq!(mixed.angles, [40.0, 50.0, 0.0]);
        assert_eq!(mixed.source, "mixed");
    }

    #[test]
    fn view_viewport_refusals_are_the_c_ones() {
        let err = |params: &str| view_viewport(Some(params), &live_camera(), shape()).unwrap_err();
        assert_eq!(
            err(r#"{"origin":[1,"x",3]}"#).data,
            r#"{"reason":"origin: [x,y,z]"}"# // :2705
        );
        assert_eq!(
            err(r#"{"angles":[1,2]}"#).data,
            r#"{"reason":"angles: [pitch,yaw,roll]"}"# // :2709
        );
        assert_eq!(err(r#"{"fov":0}"#).data, r#"{"reason":"fov: (1, 179) degrees"}"#); // :2717
        assert_eq!(
            err(r#"{"znear":0}"#).data,
            r#"{"reason":"znear/zfar"}"# // :2718
        );
        assert_eq!(
            err(r#"{"name":"../x"}"#).data,
            r#"{"reason":"name: [A-Za-z0-9._-], 1..64 chars, no leading dot"}"# // :2729
        );
    }

    #[test]
    fn view_viewport_reads_the_flags_and_overrides() {
        let p = view_viewport(
            Some(
                r#"{"width":320,"height":180,"format":2,"fov":60,"znear":0.5,"zfar":100,
                    "srgb":true,"hideToolMaterials":true,"name":"x.png"}"#,
            ),
            &live_camera(),
            ViewShape { w: 320, h: 180, format: 2 },
        )
        .expect("valid");
        assert_eq!((p.w, p.h, p.format), (320, 180, 2));
        assert_eq!((p.fov, p.znear, p.zfar), (60.0, 0.5, 100.0));
        assert!(p.srgb); // format 2 defaults to false, the request re-enables it
        assert!(p.hide_tools);
        assert_eq!(p.stem, b"x");
        // format 2 with no `srgb` member keeps the C's `lfmt == 0` default.
        let p = view_viewport(
            Some(r#"{"format":2}"#),
            &live_camera(),
            ViewShape { w: 640, h: 360, format: 2 },
        )
        .expect("valid");
        assert!(!p.srgb);
    }

    // -----------------------------------------------------------------------------------------
    // world.render's parameters (`mcp_bridge.c:2929-2948`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn world_params_require_origin_and_angles() {
        assert_eq!(
            world_params(None).unwrap_err().data,
            r#"{"reason":"origin: [x,y,z] required"}"# // :2932
        );
        assert_eq!(
            world_params(Some(r#"{"angles":[1,2,3]}"#))
                .unwrap_err()
                .data,
            r#"{"reason":"origin: [x,y,z] required"}"#
        );
        assert_eq!(
            world_params(Some(r#"{"origin":[1,2,3]}"#))
                .unwrap_err()
                .data,
            r#"{"reason":"angles: [pitch,yaw,roll] required"}"# // :2933
        );
    }

    #[test]
    fn world_params_defaults_and_bounds() {
        let p = world_params(Some(r#"{"origin":[1,2,3],"angles":[4,5,6]}"#)).expect("valid");
        assert_eq!((p.w, p.h), (1920, 1080)); // :2930
        assert_eq!((p.fov, p.znear, p.zfar), (90.0, 4.0, 30000.0)); // :2931
        assert!(!p.srgb); // :2942
        assert!(p.stem.starts_with(b"render_"), "{:?}", p.stem);
        let err = |params: &str| world_params(Some(params)).unwrap_err().data;
        assert_eq!(
            err(r#"{"origin":[1,2,3],"angles":[4,5,6],"width":8192,"height":8192}"#),
            r#"{"reason":"width/height: 16..8192 each, at most 33554432 pixels"}"# // :2937
        );
        assert_eq!(
            err(r#"{"origin":[1,2,3],"angles":[4,5,6],"fov":179}"#),
            r#"{"reason":"fov: (1, 179) degrees, horizontal"}"# // :2940
        );
        assert_eq!(
            err(r#"{"origin":[1,2,3],"angles":[4,5,6],"zfar":2000000}"#),
            r#"{"reason":"znear/zfar"}"# // :2941
        );
        assert_eq!(
            err(r#"{"origin":[1,2,3],"angles":[4,5,6],"name":".x"}"#),
            r#"{"reason":"name: [A-Za-z0-9._-], 1..64 chars, no leading dot"}"# // :2946
        );
    }

    // -----------------------------------------------------------------------------------------
    // The ToolScene contract (`mcp_bridge_pet_render.inc:53-72`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn toolscene_params_defaults_and_refusals() {
        let (w, h, stem) = toolscene_params(None).expect("defaults");
        assert_eq!((w, h), (1024, 768)); // :59
        assert!(stem.starts_with(b"render_"), "{:?}", stem);
        let err = |params: &str| toolscene_params(Some(params)).unwrap_err().data;
        assert_eq!(
            err(r#"{"width":8}"#),
            r#"{"reason":"width/height: 16..4096 each, at most 8388608 pixels"}"# // :62
        );
        assert_eq!(
            err(r#"{"width":4096,"height":4096}"#),
            r#"{"reason":"width/height: 16..4096 each, at most 8388608 pixels"}"#
        );
        assert_eq!(
            err(r#"{"name":".x"}"#),
            r#"{"reason":"name: [A-Za-z0-9._-], 1..64 chars, no leading dot"}"# // :66
        );
    }

    /// `pet.render`'s arm tail is the C's pet tail (`mcp_bridge.c:6222-6231`); this lane spells
    /// it over `Sb` because the reply carries `meanLuma`, which the pet lane's `Buf` does not
    /// build. The two must answer the same bytes for the same content, all four arms included.
    #[test]
    fn the_pet_arm_tail_matches_the_pet_lane() {
        use crate::ops::pet::{buffer, finish_pet};

        let err = OpErr::new(-32602, "InvalidParams", "bad \"x\"");
        let mut b = buffer();
        b.add(b"{\"a\":1}");
        let mut buf = vec![0u8; crate::control::MAX_LINE as usize - 2048];
        let mut s = Sb::new(&mut buf);
        s.add(b"{\"a\":1}");
        for outcome in [
            Ok(()),
            Err(err.clone()),
            Err(OpErr::bare(-32003, "NoActiveDocument")),
        ] {
            assert_eq!(finish_pet("id", outcome.clone(), &b), finish_pet_sb("id", outcome, &s));
        }
        // The overflow arm: a builder too small for its object.
        let mut small_buf = vec![0u8; 8];
        let mut small_sb = Sb::new(&mut small_buf);
        small_sb.add(b"{\"a\":12345}");
        let mut small_pet = crate::ops::pet::reply::Buf::new(8);
        small_pet.add(b"{\"a\":12345}");
        assert_eq!(
            finish_pet("id", Ok(()), &small_pet),
            finish_pet_sb("id", Ok(()), &small_sb),
            "the -32603 overflow answer is the same message"
        );
    }

    // -----------------------------------------------------------------------------------------
    // The stats block (`mcp_bridge.c:2845-2852`)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn add_stats_writes_the_c_object() {
        let stats = Stats {
            untouched: 0,
            luma_sum: 25,
            non_black: 2,
            luma_min: 1,
            luma_max: 24,
        };
        let mut buf = vec![0u8; 256];
        let mut b = sb(&mut buf);
        add_stats(&mut b, &stats, 2, 0xDEAD_BEEF);
        assert_eq!(
            b.as_cstr(),
            br#","stats":{"untouchedPixels":0,"nonBlackPixels":2,"meanLuma":12.500,"minLuma":1,"maxLuma":24,"crc32":"deadbeef"}"#
        );
    }

    /// The renders directory rule: explicit, then the container's `Z:\app`, then
    /// `%LOCALAPPDATA%\cs2mcpbridge` (FIXME 16: a native install has no `Z:\app`).
    #[test]
    fn renders_dir_rule() {
        let local = Some(r"C:\Users\tester\AppData\Local");
        assert_eq!(
            resolve_renders_dir(Some(r"D:\out\renders\"), true, local),
            br"D:\out\renders\".to_vec()
        );
        assert_eq!(resolve_renders_dir(Some("D:/x/"), false, None), br"D:/x\".to_vec());
        assert_eq!(resolve_renders_dir(None, true, local), WINE_RENDERS_DIR.to_vec());
        assert_eq!(
            resolve_renders_dir(Some(""), false, local),
            br"C:\Users\tester\AppData\Local\cs2mcpbridge\renders\".to_vec()
        );
        // non-ASCII is used as it is (FIXME 16: the write is wide-char), and nothing at all
        // keeps the C's directory
        assert_eq!(
            resolve_renders_dir(None, false, Some(r"C:\Users\Jörg\AppData\Local")),
            "C:\\Users\\Jörg\\AppData\\Local\\cs2mcpbridge\\renders\\".as_bytes().to_vec()
        );
        assert_eq!(resolve_renders_dir(None, false, None), WINE_RENDERS_DIR.to_vec());
        assert_eq!(write_failed_reason(WINE_RENDERS_DIR), r"could not write Z:\app\renders");
    }
}
