//! The map lifecycle group (`map.open`, `map.reload`, `map.save`): the C bridge's
//! `bridge/mcp_bridge_map.inc` and the three dispatch arms `bridge/mcp_bridge.c:6083-6233`,
//! ported method for method onto the HAMMER-R core ([`crate::ops::hammer`]).
//!
//! `map.run` is a blocked row (`registry.rs`, `implemented: false`) and is deliberately not
//! registered here, like in the C's table.
//!
//! # Shape
//!
//! * The pure layer below: the parameter gates, the host/header file gates over injected
//!   facts, `md_str` (`mcp_bridge.c:5169-5186`), `expect_doc_ok`, and every result builder -
//!   plain functions over plain inputs, so the host tests drive them and compare the C's
//!   `sb_add` sequence byte for byte.
//! * [`Game`] - the thin reads and calls the three bodies need from the live Hammer app (the
//!   session walk of `map_session_of`, the `OpenFile` / `ReloadFromDisk` / `Save` vtable
//!   calls, `DocOrDependencyChanged` through the bind table), with [`hammer::game`]-backed
//!   defaults and a non-Windows refusal object, exactly like [`crate::ops::hammer::Hammer`].
//! * The three handlers - the C's arms, gate by gate, in the C's order.
//!
//! Everything runs on the game's main thread (the tick's drain), like the C. A panic in a
//! body is caught by the drain and answered `-32603`.

use core::ffi::c_void;

use crate::control;
use crate::entity_json::{json_find, json_string, Sb};
use crate::generated::pins;
use crate::kv3_check;
use crate::map_path;
use crate::ops::hammer::{self, Doc};
use crate::ops::ident::obj_is;
use crate::ops::pet::game as pet_game;
use crate::ops::{finish, OpErr};
use crate::queue::Request;

/// The C's `static char rbuf[2048]` for `map.open` (`mcp_bridge.c:6084`: "the path + epoch;
/// fixed shape (m2.16)").
const OPEN_RESULT_CAP: usize = 2048;

/// The C's `char spath[1024]` / `openPath[1024]`
/// (`mcp_bridge_map.inc:206`, `:220`, `:276`).
const SESSION_PATH_CAP: usize = 1024;

/// The C's `char pathbuf[512]` for the reload reply (`mcp_bridge.c:6141`).
const RELOAD_PATH_CAP: usize = 512;

/// `map.save`'s reply path has NO C buffer: the C hands the doc's raw `char *` straight to
/// `sb_addjson` (`mcp_bridge.c:6204`), reading to the NUL unguarded. This port probes it with
/// [`md_str`] at this cap - the same 4096 cap the hammer lane's `doc_facts` chose for the
/// same shape (`bridge/protocol-diff-m3-hammer.md` records it there). A path beyond 4095
/// bytes answers cut instead of whole; no Hammer-resolved path is.
const SAVE_PATH_PROBE: usize = 4096;

/// The C string view of `bytes`: everything before the first NUL.
fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&byte| byte == 0) {
        Some(end) => &bytes[..end],
        None => bytes,
    }
}

// -------------------------------------------------------------------------------------------
// `md_str` (`mcp_bridge.c:5169-5186`)
// -------------------------------------------------------------------------------------------

/// `md_str` (`mcp_bridge.c:5169-5186`): copy a game-owned C string, probing readability first
/// and again at every 4 KB page boundary (strings can point at freed or reallocated
/// `CUtlString` storage). Returns 1 when the whole string fit, -1 when it was cut at `cap-1`
/// (the prefix stays in `out`), 0 when null or unreadable (`out[0] == 0`).
///
/// This is the pet lane's `md_str` made cross-platform (pet's lives in its `cfg(windows)`
/// reader); the hammer lane's [`hammer::probe_str`] is NOT used because it answers
/// differently where it matters here: a string of exactly `cap-1` bytes (probe_str says 1,
/// `md_str` -1, which makes [`Game::session_path`] skip the session) and at a cut code point
/// (probe_str trims to a UTF-8 boundary, `md_str` keeps the raw bytes).
///
/// # Safety
///
/// `s` is a game pointer: it is only ever *read* through `mem_ok`, which reports an
/// unreadable address instead of faulting on both targets (the C's `ReadProcessMemory`).
pub unsafe fn md_str(s: *const u8, out: &mut [u8]) -> i32 {
    let cap = out.len();
    if cap == 0 {
        return 0;
    }
    out[0] = 0;
    if s.is_null() || !crate::ops::ident::mem_ok(s.cast()) {
        return 0;
    }
    let mut i = 0usize;
    while i + 1 < cap {
        // Every page boundary is re-probed (`:5178`).
        if i != 0
            && (s as usize).wrapping_add(i) & 0xFFF == 0
            && !crate::ops::ident::mem_ok(s.wrapping_add(i).cast())
        {
            out[0] = 0;
            return 0;
        }
        // SAFETY: `mem_ok` proved the first byte and every page boundary so far; the C reads
        // `s[i]` unguarded between probes (`:5180`).
        let byte = unsafe { s.add(i).read() };
        out[i] = byte;
        if byte == 0 {
            return 1;
        }
        i += 1;
    }
    out[cap - 1] = 0;
    -1
}

// -------------------------------------------------------------------------------------------
// The parameter gates (`op_map_open`'s head, `mcp_bridge_map.inc:226-239`)
// -------------------------------------------------------------------------------------------

/// `map.open`'s `path` member, validated: the three param gates run before anything touches a
/// file or the game, exactly as the C runs them.
///
/// 1. the member must be a non-empty JSON string that fits the C's `char path[MAP_PATH_MAX+8]`
///    (`mcp_bridge_map.inc:227-233`);
/// 2. no `\` escape may survive in the RAW token (`:235-238` - `json_string` collapses
///    escapes, so the raw scan runs on [`json_find`]'s value, which points at the opening
///    quote);
/// 3. the canonical shape gate (`map_open_path_ok`, `map_path.h:79`): the C's refusal reason
///    string goes out as the `-32602` data.
pub fn params_gate(params: Option<&str>) -> Result<Vec<u8>, OpErr> {
    // `char path[MAP_PATH_MAX + 8]` (`mcp_bridge_map.inc:220`).
    let mut path = [0u8; map_path::MAP_PATH_MAX + 8];
    let Some(params) = params else {
        return Err(missing_path_err()); // `!params` (:227)
    };
    // `!json_string(params, "path", path, sizeof path) || !path[0]` (:227-233).
    if !json_string(params.as_bytes(), b"path", &mut path) || path[0] == 0 {
        return Err(missing_path_err());
    }
    // the raw-token escape scan (:235-238).
    if let Some(raw) = json_find(params.as_bytes(), b"path") {
        for &byte in &params.as_bytes()[raw + 1..] {
            if byte == b'"' {
                break;
            }
            if byte == b'\\' {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "path: no JSON escapes (use '/' separators)",
                ));
            }
        }
    }
    // `map_open_path_ok(path, &reason)` (:239): the C's own reason string in the data.
    map_path::open_path_ok(cstr(&path))
        .map_err(|reason| OpErr::new(-32602, "InvalidParams", reason.as_str()))?;
    Ok(cstr(&path).to_vec())
}

/// The shared refusal of `!params` / `!json_string` / `!path[0]` (`mcp_bridge_map.inc:227-233`).
fn missing_path_err() -> OpErr {
    OpErr::new(
        -32602,
        "InvalidParams",
        "path: relative to the addon content root (maps/... .vmap), 1..512 chars, no '..'",
    )
}

// -------------------------------------------------------------------------------------------
// The host-file gate (`map_host_gate`, `mcp_bridge_map.inc:74-112`)
// -------------------------------------------------------------------------------------------

/// What the file probe answered for the derived host path. `CreateFileA` +
/// `GetFileType` in production (`mcp_bridge_map.inc:97-105`); a closure in the tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFact {
    /// A disk file (the C's `GetFileType(f) == PET_FILE_TYPE_DISK`).
    Disk,
    /// `CreateFileA` failed (`no_such_file`).
    NoSuchFile,
    /// The handle opened but is a pipe/device (`not_a_file`).
    NotAFile,
}

/// `map_host_gate` (`mcp_bridge_map.inc:74-112`) over injected process facts and one file
/// probe, with the C's buffer sizes: `app[1024]`, `addon[64]`, `host[2048]`.
///
/// Returns the derived host path bytes (`<app>\content\csgo_addons\<addon>\<rel>`), or the
/// C's error: `-32017 PrerequisiteFailed` for the two process facts, `-32603
/// host_path_too_long` (unreachable with these caps, the C answers it anyway), `-32014
/// AssetNotFound` for the two file refusals.
pub fn host_gate_with(
    app: Option<&str>,
    addon: Option<&str>,
    rel: &[u8],
    probe: impl FnOnce(&[u8]) -> HostFact,
) -> Result<Vec<u8>, OpErr> {
    // `editor_app_root(app, sizeof app)` (:78-83) - the app buffer is the C's 1024.
    let Some(exe) = app else {
        crate::log::line("map: open refused no_app_root");
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "no_app_root"));
    };
    let mut appbuf = [0u8; pet_game::APP_CAP];
    if !kv3_check::app_root_into(exe, &mut appbuf) {
        crate::log::line("map: open refused no_app_root");
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "no_app_root"));
    }
    // `kv3_addon_name(GetCommandLineA(), addon, sizeof addon)` (:84-89).
    let Some(addon) = addon else {
        crate::log::line("map: open refused no_addon");
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "no_addon"));
    };
    let mut addonbuf = [0u8; pet_game::ADDON_CAP];
    if !kv3_check::addon_name_into(addon, &mut addonbuf) {
        crate::log::line("map: open refused no_addon");
        return Err(OpErr::new(-32017, "PrerequisiteFailed", "no_addon"));
    }
    // `kv3_host_path(app, addon, rel, host, hostcap)` (:90-96): unreachable with these caps
    // (app <= 1023, addon <= 63, rel <= 512), the C answers it anyway.
    let mut host = [0u8; pet_game::HOST_CAP];
    // The path bytes are JSON-decoded from a Rust `String`, so they are valid UTF-8; the
    // `*_into` helpers copy a `&str` prefix.
    let rel = core::str::from_utf8(rel)
        .expect("a JSON-decoded path is valid UTF-8; json_string only collapses escapes");
    if !kv3_check::host_path_into(
        core::str::from_utf8(cstr(&appbuf)).expect("the kv3_check *_into helpers copy a &str prefix at an ASCII separator"),
        core::str::from_utf8(cstr(&addonbuf)).expect("the kv3_check *_into helpers copy a &str prefix at an ASCII separator"),
        rel,
        &mut host,
    ) {
        return Err(OpErr::new(-32603, "InternalError", "host_path_too_long"));
    }
    let host = cstr(&host).to_vec();
    // the file probe (:97-107).
    match probe(&host) {
        HostFact::NoSuchFile => {
            crate::log::line("map: open refused no_such_file "); // :99-100
            crate::log::line(&String::from_utf8_lossy(&host));
            Err(OpErr::new(-32014, "AssetNotFound", "no_such_file"))
        }
        HostFact::NotAFile => {
            crate::log::line("map: open refused not_a_file"); // :103-104
            Err(OpErr::new(-32014, "AssetNotFound", "not_a_file"))
        }
        HostFact::Disk => Ok(host),
    }
}

// -------------------------------------------------------------------------------------------
// The content gate (`map_header_gate`, `mcp_bridge_map.inc:121-152`)
// -------------------------------------------------------------------------------------------

/// `map_header_gate` (`mcp_bridge_map.inc:121-152`) over one injected reader: `read` fills the
/// C's `char head[MAP_VMAP_HEADER_MAX]` window (`None` = `CreateFileA` failed, the file
/// vanished between the gates) and answers how many bytes it got (`Some(0)` = the C's
/// `ReadFile` failure: `if (!ReadFile(...)) got = 0`).
///
/// A file whose first bytes do not carry the binary-v9 DMX header is refused `-32014
/// AssetNotFound {"reason":"InvalidMapFile"}` BEFORE OpenFile can pop its modal error box.
pub fn header_gate_with(
    host: &[u8],
    read: impl FnOnce(&mut [u8]) -> Option<u32>,
) -> Result<(), OpErr> {
    let mut head = [0u8; map_path::MAP_VMAP_HEADER_MAX];
    let Some(got) = read(&mut head) else {
        crate::log::line("map: open refused header gate no file "); // :133-136
        crate::log::line(&String::from_utf8_lossy(host));
        return Err(OpErr::new(-32014, "AssetNotFound", "no_such_file"));
    };
    // `if (!got || !map_vmap_header_ok(head, (u32)got, &why))` (:140-149): a zero read takes
    // the same refusal (the C's `why` still names the last probe; the log only needs to be
    // truthful about the file, which it is).
    if got == 0 {
        crate::log::line(&format!(
            "map: open refused, not a binary-v9 vmap: (no bytes read) {}",
            String::from_utf8_lossy(host)
        ));
        return Err(OpErr::new(-32014, "AssetNotFound", "InvalidMapFile"));
    }
    if let Err(reason) = map_path::vmap_header_ok(&head[..got as usize]) {
        crate::log::line(&format!(
            "map: open refused, not a binary-v9 vmap: {} {}",
            reason.as_str(),
            String::from_utf8_lossy(host)
        )); // :142-146
        return Err(OpErr::new(-32014, "AssetNotFound", "InvalidMapFile"));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// `expect_doc_ok` (`mcp_bridge.c:1920-1938`)
// -------------------------------------------------------------------------------------------

/// `expect_doc_ok` (`mcp_bridge.c:1920-1938`) over the fingerprint state the caller read
/// ([`hammer::fingerprint_ids`], the C's `g_fp`): the `expectDoc {id, epoch}` member must name
/// THIS document and epoch, or the arm refuses `-32004 DocumentChanged` with the C's `why`.
/// The one port is [`hammer::expect_doc_ok`]; this adapts map's `&str` params.
fn expect_doc_ok(params: Option<&str>, doc_seq: u32, epoch: u32) -> Result<(), &'static str> {
    hammer::expect_doc_ok(params.map(str::as_bytes), doc_seq, epoch)
}

// -------------------------------------------------------------------------------------------
// The result builders - pure, byte-compared against the C's `sb_add` sequence in the tests
// -------------------------------------------------------------------------------------------

/// `map.open`'s already-open result (`mcp_bridge_map.inc:252-255`): `path` is the session's
/// own path text when it could be read, else the requested path (the caller's selection).
pub fn write_existing_result(out: &mut Sb, path: &[u8], active: bool, epoch: u32) {
    out.add(b"{\"opened\":false,\"existing\":true,\"path\":");
    out.addjson(path);
    out.add(b",\"active\":");
    out.add(if active { b"true" } else { b"false" });
    out.add(b",\"epoch\":");
    out.addnum(u64::from(epoch));
    out.add(b"}");
}

/// `map.open`'s OpenFile result (`mcp_bridge_map.inc:289-297`): `path` is the new session's
/// path text when one was read, else the requested path; `opened:false` carries the C's
/// `"reason":"open_failed"` (the re-entrancy guard or a load failure).
pub fn write_open_result(out: &mut Sb, opened: bool, path: &[u8], epoch: u32) {
    out.add(b"{\"opened\":");
    out.add(if opened { b"true" } else { b"false" });
    out.add(b",\"existing\":false,\"path\":");
    out.addjson(path);
    if !opened {
        out.add(b",\"reason\":\"open_failed\"");
    }
    out.add(b",\"epoch\":");
    out.addnum(u64::from(epoch));
    out.add(b"}");
}

/// `map.reload`'s whole success reply (`mcp_bridge.c:6157-6165`): the arm builds the envelope
/// inline; `path` is empty when the pre-reload copy could not read the doc's text.
pub fn reload_reply(id: &str, path: &[u8], discarded_changes: bool, epoch: u32) -> String {
    let mut buf = vec![0u8; control::MAX_LINE as usize];
    let mut b = Sb::new(&mut buf);
    b.add(b"{\"jsonrpc\":\"2.0\",\"id\":");
    b.addjson(id.as_bytes());
    b.add(b",\"bridge\":{\"protocol\":1,\"build\":\"");
    b.add(crate::BRIDGE_BUILD.as_bytes());
    b.add(b"\"},\"result\":{\"reloaded\":true,\"path\":");
    b.addjson(path);
    b.add(b",\"discardedChanges\":");
    b.add(if discarded_changes { b"true" } else { b"false" });
    b.add(b",\"epoch\":");
    b.addnum(u64::from(epoch));
    b.add(b"}}\n");
    if b.ovf() {
        crate::log::line("exec: response buffer overflow"); // the arm has no overflow arm; logged like doc.active's port
    }
    String::from_utf8_lossy(b.as_cstr()).into_owned()
}

/// `map.save`'s whole success reply (`mcp_bridge.c:6199-6206`), `skipped_clean` true when the
/// document was clean and Save was NOT called.
pub fn save_reply(id: &str, path: &[u8], saved: bool, skipped_clean: bool) -> String {
    let mut buf = vec![0u8; control::MAX_LINE as usize];
    let mut b = Sb::new(&mut buf);
    b.add(b"{\"jsonrpc\":\"2.0\",\"id\":");
    b.addjson(id.as_bytes());
    b.add(b",\"bridge\":{\"protocol\":1,\"build\":\"");
    b.add(crate::BRIDGE_BUILD.as_bytes());
    b.add(b"\"},\"result\":{\"saved\":");
    b.add(if saved { b"true" } else { b"false" });
    b.add(b",\"path\":");
    b.addjson(path);
    b.add(b",\"skippedClean\":");
    b.add(if skipped_clean { b"true" } else { b"false" });
    b.add(b"}}\n");
    if b.ovf() {
        crate::log::line("exec: response buffer overflow");
    }
    String::from_utf8_lossy(b.as_cstr()).into_owned()
}

// -------------------------------------------------------------------------------------------
// The reader contract (`Game`) and its two implementations
// -------------------------------------------------------------------------------------------

/// `map_session_of`'s answer (`mcp_bridge_map.inc:154-211`).
// The Windows reader constructs every variant; the Linux lib build sees none of them.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionOf {
    /// No session holds the file (or the vector is empty).
    None,
    /// The session that already holds `host`: its address, its path text (raw bytes), and
    /// whether it is the app's active session.
    Found {
        session: usize,
        path: Vec<u8>,
        active: bool,
    },
    /// The vector could not be walked at all - the caller refuses rather than call OpenFile
    /// with the leak gate switched off (the C's `*badOut = 1`).
    Unreadable,
}

/// The doc-session facts `map.reload` / `map.save` read (`mcp_bridge.c:6116-6119`,
/// `:6180-6183`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DocSessionFacts {
    /// `CMapDocSession_Owner(docSession)`; 0 when there is none (then `is_new` is 1).
    pub session: usize,
    /// `session ? session->m_bIsNew : 1`.
    pub is_new: bool,
    /// `CMapDocSession_Path(docSession)`: the doc path text's raw address (0 = none).
    pub path_ptr: usize,
    /// `*pathp` when `path_ptr` is live and readable, else 0. The C derefs `*pathp` unguarded
    /// (`mcp_bridge.c:6186`); this port probes the byte with `mem_ok` first - a memory-safety
    /// fix that cannot change a byte for any readable path (protocol-diff).
    pub path_first: u8,
}

/// The live reads and calls the three map bodies need, over the same objects
/// [`crate::ops::hammer`]'s readers walk. Main thread only.
pub(crate) trait Game {
    /// `resolve_hammer()` (`mcp_bridge.c:1693-1711`), through the hammer lane's reader.
    fn app(&self) -> Option<usize> {
        hammer::game().app()
    }

    /// `active_doc` (`mcp_bridge.c:1713-1735`), through the hammer lane's reader.
    fn active_doc(&self) -> Result<Doc, &'static str> {
        hammer::game().active_doc()
    }

    /// `doc_editable` (`mcp_bridge.c:1780-1802`), through the hammer lane's reader.
    fn doc_editable(&self, doc: Doc) -> (bool, &'static str) {
        let chain: &dyn hammer::Chain = hammer::game();
        hammer::doc_editable_with(chain, doc)
    }

    /// `map_session_of` (`mcp_bridge_map.inc:154-211`): walk the app's session vector and
    /// find the session whose doc path IS `host`.
    fn session_of(&self, app: usize, host: &[u8]) -> SessionOf;

    /// `map_session_path` (`mcp_bridge_map.inc:36-52`): the session's doc path TEXT, probed
    /// with `md_str`'s semantics, into `out` (the C's `char out[1024]`). `false` = no doc /
    /// empty / unreadable / cut - an unusable path is not an error (review L6).
    fn session_path(&self, session: usize, out: &mut [u8]) -> bool;

    /// `CHammerApp_ActiveSession` (`bridge/s2sdk.h:340`).
    fn active_session(&self, app: usize) -> usize;

    /// The reload/save doc facts (`mcp_bridge.c:6116-6119`).
    fn doc_session_facts(&self, doc: Doc) -> DocSessionFacts;

    /// `g_hfn.DocOrDependencyChanged(doc, 0)` (`mcp_bridge.c:6120`); false when the table is not
    /// bound (the hammer lane's `undo_history` answers the same).
    fn doc_or_dependency_changed(&self, doc: Doc) -> bool;

    /// `session->vt->ReloadFromDisk(session)` (`mcp_bridge.c:6155`): `void`, no return value
    /// to read (review M1).
    fn reload_from_disk(&self, session: usize);

    /// `session->vt->Save(session, pathp, pathp, 0, 0)` (`mcp_bridge.c:6191`): the one-byte
    /// success flag, read as a byte (never a Rust `bool` out of game code).
    fn save(&self, session: usize, path_ptr: usize) -> bool;

    /// `app->vt->OpenFile(app, path, 1)` (`mcp_bridge_map.inc:268`): the new session, 0 on
    /// failure / the re-entrancy guard.
    fn open_file(&self, app: usize, path: &[u8]) -> usize;
}

/// The production reader on the Windows tools build.
#[cfg(windows)]
static LIVE: LiveMap = LiveMap;

/// The absent-platform reader: there is no Hammer module (the tools DLL only ever runs on
/// Windows), so the method gate answers `-32017` before a body starts. Anything that still
/// reaches a read fails loudly instead of inventing an answer - the trait defaults delegate
/// to [`hammer::game`], whose `NoHammer` refuses; the raw reads have nothing to read.
#[cfg(not(windows))]
static LIVE: NoMap = NoMap;

#[cfg(not(windows))]
struct NoMap;

#[cfg(not(windows))]
impl Game for NoMap {
    fn session_of(&self, _app: usize, _host: &[u8]) -> SessionOf {
        SessionOf::None
    }
    fn session_path(&self, _session: usize, _out: &mut [u8]) -> bool {
        false
    }
    fn active_session(&self, _app: usize) -> usize {
        0
    }
    fn doc_session_facts(&self, _doc: Doc) -> DocSessionFacts {
        DocSessionFacts {
            session: 0,
            is_new: true,
            path_ptr: 0,
            path_first: 0,
        }
    }
    fn doc_or_dependency_changed(&self, _doc: Doc) -> bool {
        false
    }
    fn reload_from_disk(&self, _session: usize) {}
    fn save(&self, _session: usize, _path_ptr: usize) -> bool {
        false
    }
    fn open_file(&self, _app: usize, _path: &[u8]) -> usize {
        0
    }
}

/// The production reader (Windows tools build only), over the hammer lane's live chain.
#[cfg(windows)]
struct LiveMap;

#[cfg(windows)]
mod live {
    use core::ffi::c_void;
    use core::mem::offset_of;
    use core::ptr;

    use s2sdk_sys::generated::{
        CBaseEditorApp, CBaseEditorSession, CHammerEditorSession, CHammerEditorSession_slots,
        CMapDocSession, CUtlVector_CEntityHandle,
    };

    use super::{cstr, Doc, DocSessionFacts, Game, LiveMap, SessionOf, SESSION_PATH_CAP};
    use crate::generated::pins;
    use crate::log;
    use crate::ops::ident::{mem_ok, obj_is};
    use crate::map_path;

    /// `CHammerApp_ActiveSession` (`s2sdk.h:340`): the generated `m_pActiveSession` member.
    ///
    /// # Safety
    ///
    /// `app` is the identity-checked `CHammerApp` (`resolve_hammer`'s checks).
    unsafe fn active_session_of(app: usize) -> usize {
        // SAFETY: see above; one generated member read, as the C's inline helper is.
        unsafe { (*(app as *const CBaseEditorApp)).m_pActiveSession as usize }
    }

    /// `map_session_path` (`mcp_bridge_map.inc:36-52`): the doc path TEXT through the
    /// docSession's generated `m_Doc.m_Path.m_pString` member (the `char *` at +0x6B0,
    /// review C1), probed, `md_str(...) == 1` required.
    ///
    /// # Safety
    ///
    /// `session` passed `obj_is(HV_SESSION_VT)`, so its `m_pDocSession` member read is valid.
    unsafe fn map_session_path(session: usize, out: &mut [u8]) -> bool {
        // SAFETY: the caller identity-checked the session; the C reads the same member (:38).
        let doc_session =
            unsafe { (*(session as *const CHammerEditorSession)).m_pDocSession };
        let p: *const u8 = if doc_session.is_null() || !mem_ok(doc_session.cast()) {
            core::ptr::null()
        } else {
            // SAFETY: `doc_session` passed `mem_ok`; `m_Doc.m_Path.m_pString` is one
            // generated member read, the C's `CMapDocSession_Path` (`s2sdk.h:391`).
            unsafe { (*(doc_session)).m_Doc.m_Path.m_pString.cast::<u8>().cast_const() }
        };
        !p.is_null()
            && mem_ok(p.cast())
            // SAFETY: `mem_ok` just proved this byte readable (`:46`: `p[0]`).
            && unsafe { p.read() } != 0
            // SAFETY: `p`'s first byte and every page boundary are probed inside (`md_str`).
            && unsafe { super::md_str(p, out) } == 1
    }

    /// The C's session-vector walk (`map_session_of`, `mcp_bridge_map.inc:154-211`), over the
    /// generated `CBaseEditorApp::m_Sessions` bytes read as the generic CUtlVector layout
    /// (`m_Size` at 0, `m_pMemory` at 8 - the same cast the hammer lane's `scope_depth` does).
    fn map_session_of(app: usize, host: &[u8]) -> SessionOf {
        // SAFETY: `app` is the identity-checked `CHammerApp` (`resolve_hammer`'s checks);
        // `m_Sessions` is the generated member the C reads (`s2sdk.h:341-343`).
        let (n, data) = unsafe {
            let vector = ptr::addr_of!((*(app as *const CBaseEditorApp)).m_Sessions)
                .cast::<CUtlVector_CEntityHandle>();
            ((*vector).m_Size as u32, (*vector).m_pMemory)
        };
        let data = data.cast::<*const CHammerEditorSession>();
        if n == 0 {
            return SessionOf::None; // `if (!n) return 0` (:169)
        }
        if n > 4096u32
            || data.is_null()
            || !mem_ok(data.cast())
            // `mem_ok(data + n - 1)`: the LAST element pointer's own bytes (:170).
            // SAFETY: `n >= 1` here; `data` itself was just proven readable.
            || !mem_ok(unsafe { data.add(n as usize - 1) }.cast())
        {
            log::line(&format!("map: session vector unreadable, count {n}")); // :171
            return SessionOf::Unreadable;
        }
        for i in 0..n as usize {
            // SAFETY: `data..data+n` was proven readable above; the C reads `data[i]` the
            // same way (:174).
            let session = unsafe { *data.add(i) };
            if session.is_null() {
                continue; // :180
            }
            if !obj_is(session.cast(), pins::PIN_HAMMER, pins::HV_SESSION_VT.value as u32) {
                log::line(&format!(
                    "map: session vector entry is not a session at index {i}"
                )); // :182-183: the layout moved - fail loudly, do not call OpenFile
                return SessionOf::Unreadable;
            }
            let mut spath = [0u8; SESSION_PATH_CAP];
            // SAFETY: `session` passed `obj_is(HV_SESSION_VT)` just above.
            if unsafe { !map_session_path(session as usize, &mut spath) } {
                log::line(&format!(
                    "map: session has no readable path (no doc / untitled / cut) at index {i}"
                )); // :190-191
                continue; // REVIEW L6: an unusable path is not an error, never was
            }
            if map_path::path_eq(&spath, host) {
                // SAFETY: `app` is the identity-checked app.
                let active = unsafe { active_session_of(app) } == session as usize;
                return SessionOf::Found {
                    session: session as usize,
                    path: cstr(&spath).to_vec(),
                    active,
                };
            }
        }
        SessionOf::None
    }

    /// Entry `slot` of the vtable `obj` points at, as a typed function pointer - the C's
    /// `void **vt = *(void ***)obj; entry = vt[slot];` (`mcp_bridge.c:6191` reads `Save`
    /// this way). Used where the generated typed field's Rust signature does not match the
    /// C header's own (`Save`'s `u8` return).
    ///
    /// # Safety
    ///
    /// `obj` must have passed its identity check (`obj_is` against the pinned
    /// `HV_SESSION_VT`), so its vptr is the pinned class vtable; `slot` must be one of
    /// `CHammerEditorSession_slots`' constants (inside its generated `VTLEN`); `T` must be
    /// the slot's own signature per the generated declaration.
    unsafe fn session_vfn<T: Copy>(obj: *const c_void, slot: usize) -> T {
        // SAFETY: the vptr lies at offset 0 of every polymorphic object (the C reads it the
        // same way), and the caller's identity check pinned it to the class's vtable.
        let vptr = unsafe { obj.cast::<*const *const c_void>().read_unaligned() };
        // The entries are pointer sized, so the index scales by 8 and not by `c_void`'s
        // one byte.
        let entries = vptr.cast::<*const c_void>();
        // SAFETY: `slot` is a generated slot constant of that class, inside its VTLEN.
        let entry = unsafe { *entries.add(slot) };
        // SAFETY: `T` is the signature of the vtable entry at `slot`, per the generated
        // declaration; the bit pattern is a function pointer of that type.
        unsafe { core::mem::transmute_copy(&entry) }
    }

    impl Game for LiveMap {
        fn session_of(&self, app: usize, host: &[u8]) -> SessionOf {
            map_session_of(app, host)
        }

        fn session_path(&self, session: usize, out: &mut [u8]) -> bool {
            // SAFETY: the callers pass a session the walk identity-checked (`obj_is`) or the
            // session OpenFile returned (its vptr is the pinned session vtable by
            // construction); the member read is the C's `s->m_pDocSession` (:38).
            unsafe { map_session_path(session, out) }
        }

        fn active_session(&self, app: usize) -> usize {
            // SAFETY: `app` is the identity-checked `CHammerApp`.
            unsafe { active_session_of(app) }
        }

        fn doc_session_facts(&self, doc: Doc) -> DocSessionFacts {
            // CMapDoc_Session (`s2sdk.h:389`): the embedding session is the document's
            // address minus the embedded member's offset (the hammer lane's `doc_facts`
            // does the same arithmetic).
            let doc_session = (doc.0 - offset_of!(CMapDocSession, m_Doc)) as *const CMapDocSession;
            // SAFETY: `doc` passed the identity chain (`active_doc`), so the session
            // embedding it is live; the C reads the same members unguarded (:6116-6119).
            unsafe {
                let owner = (*doc_session).m_pOwner;
                let is_new = if owner.is_null() {
                    true
                } else {
                    // The owner is the active session `active_doc` identity-checked (its
                    // backptr check); `m_bIsNew` is a one-byte MSVC `bool`, read as a byte so
                    // a garbage value cannot form a Rust `bool` (`mcp_bridge.c:6118`).
                    ptr::addr_of!((*owner).base.m_bIsNew)
                        .cast::<u8>()
                        .read_unaligned()
                        != 0
                };
                let path_ptr = (*doc_session).m_Doc.m_Path.m_pString;
                let path_first = if path_ptr.is_null() {
                    0
                } else if mem_ok(path_ptr.cast()) {
                    // SAFETY: `mem_ok` just proved the byte readable; the C reads `*pathp`
                    // unguarded (:6186) - a memory-safety fix, no wire change.
                    path_ptr.cast::<u8>().read()
                } else {
                    0
                };
                DocSessionFacts {
                    session: owner as usize,
                    is_new,
                    path_ptr: path_ptr as usize,
                    path_first,
                }
            }
        }

        fn doc_or_dependency_changed(&self, doc: Doc) -> bool {
            let Some(table) = crate::ops::hammer::bind::hfn() else {
                return false; // as the hammer lane's `undo_history` answers before the bind
            };
            // SAFETY: the verified `HFN_DOC_OR_DEPENDENCY_CHANGED` function, called with the
            // document the C passes (`mcp_bridge.c:6120`).
            unsafe { (table.doc_or_dependency_changed)(doc.0 as *mut c_void, 0) != 0 }
        }

        fn reload_from_disk(&self, session: usize) {
            // SAFETY: `session` is the owner the identity chain verified (`active_doc`'s
            // backptr check); `ReloadDocument` is the generated typed field of
            // `CBaseEditorSession_vtable` at `S2_SLOT_CBaseEditorSession_ReloadDocument` (7),
            // and the C declares the slot `void` (`s2sdk.h:353`) - no return value is read.
            unsafe {
                let session = session as *const CHammerEditorSession;
                match CHammerEditorSession::vtable(session).base.ReloadDocument {
                    Some(f) => f(session.cast_mut().cast()),
                    None => panic!("map: ReloadDocument slot missing from the generated vtable"),
                }
            }
        }

        fn save(&self, session: usize, path_ptr: usize) -> bool {
            // The C header's own signature (`bridge/s2sdk.h:355-360`):
            // `u8 (*Save)(this, pszOldPath, pszNewPath, u8, u8)`. The generated typed field
            // returns a Rust `bool`, but the C reads `u8 rc` and tests `rc ? 1 : 0`
            // (`mcp_bridge.c:6191-6192`) - a Rust `bool` cannot hold a flag byte other
            // than 0/1, so the entry is read through the generated slot index instead.
            type SaveFn = unsafe extern "C" fn(
                *mut CBaseEditorSession,
                *const core::ffi::c_char,
                *const core::ffi::c_char,
                u8,
                u8,
            ) -> u8;
            let path = path_ptr as *const core::ffi::c_char;
            // SAFETY: as `reload_from_disk`; the session's vptr was pinned by the walk's
            // `obj_is(HV_SESSION_VT)` (or it is the session OpenFile returned, whose vptr is
            // the pinned session vtable by construction). The slot is the generated
            // `CHammerEditorSession_slots::Save` (18, the C's
            // `S2_SLOT_CBaseEditorSession_Save`), the two `u8` arguments are the C's
            // `(pathp, pathp, 0, 0)`, and the transmuted signature is the C header's own
            // one-byte flag, tested `!= 0`.
            unsafe {
                let f: SaveFn = session_vfn(
                    session as *const c_void,
                    CHammerEditorSession_slots::Save,
                );
                f(session as *mut CBaseEditorSession, path, path, 0, 0) != 0
            }
        }

        fn open_file(&self, app: usize, path: &[u8]) -> usize {
            let Ok(path) = std::ffi::CString::new(path.to_vec()) else {
                // A validated path carries no interior NUL; this cannot happen.
                return 0;
            };
            // SAFETY: `app` is the identity-checked `CHammerApp`; `OpenFile` is the generated
            // typed field of `CBaseEditorApp_vtable` at `S2_SLOT_CBaseEditorApp_OpenFile`
            // (18), and flags 1 is what the UI passes (`mcp_bridge_map.inc:268`).
            unsafe {
                let app = app as *const CBaseEditorApp;
                match CBaseEditorApp::vtable(app).OpenFile {
                    Some(f) => f(app.cast_mut(), path.as_ptr(), 1) as usize,
                    None => panic!("map: OpenFile slot missing from the generated vtable"),
                }
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// `op_map_open` (`mcp_bridge_map.inc:215-272`)
// -------------------------------------------------------------------------------------------

/// `op_map_open` (`mcp_bridge_map.inc:215-272`) over injected host facts and file probes (the
/// production entry [`op_map_open`] passes the process's own).
fn op_map_open_gated(
    game: &dyn Game,
    app: Option<&str>,
    addon: Option<&str>,
    file: impl FnOnce(&[u8]) -> HostFact,
    header: impl FnOnce(&[u8]) -> Result<(), OpErr>,
    params: Option<&str>,
    out: &mut Sb,
) -> Result<(), OpErr> {
    // gates 1-3, in the C's order, before anything touches the game (:226-241).
    let path = params_gate(params)?;
    let host = host_gate_with(app, addon, &path, file)?;
    header(&host)?;
    // the app, after the file gates (:241-242).
    let Some(app) = game.app() else {
        return Err(OpErr::new(-32003, "NoActiveDocument", "hammer_unavailable"));
    };
    // the session-leak gate (:243-256).
    match game.session_of(app, &host) {
        SessionOf::Unreadable => Err(OpErr::new(
            -32603,
            "InternalError",
            "session_vector_unreadable",
        )),
        SessionOf::Found { session, active, .. } => {
            // the C re-reads the session's own path text; when that is unusable the requested
            // path names the same file (REVIEW C1, :245-248).
            let mut buf = [0u8; SESSION_PATH_CAP];
            let selected: &[u8] = if game.session_path(session, &mut buf) {
                cstr(&buf)
            } else {
                &path
            };
            hammer::doc_fingerprint(); // :249
            let (_, epoch) = hammer::fingerprint_ids();
            crate::log::line(&format!(
                "map: already open {}",
                String::from_utf8_lossy(selected)
            )); // :250-251
            write_existing_result(out, selected, active, epoch); // :252-255
            Ok(())
        }
        SessionOf::None => {
            crate::log::line("map: open "); // :257-262: both spellings in the log
            crate::log::line(&String::from_utf8_lossy(&path));
            crate::log::line(" -> host ");
            crate::log::line(&String::from_utf8_lossy(&host));
            let opened = game.open_file(app, &path); // :268: flags 1
            crate::log::line(&format!("map: open rc {}", opened as i64 as u64)); // :269
            hammer::doc_fingerprint(); // :270: a new session changes the fingerprint
            let (_, epoch) = hammer::fingerprint_ids();
            // The session OpenFile RETURNED is the precise answer; the active session is the
            // fallback (REVIEW L6/C1, :273-284).
            let mut buf = [0u8; SESSION_PATH_CAP];
            let mut open_path: Vec<u8> = Vec::new();
            if opened != 0 && !game.session_path(opened, &mut buf) {
                let active = game.active_session(app);
                if active != 0
                    && obj_is(
                        active as *mut c_void,
                        pins::PIN_HAMMER,
                        pins::HV_SESSION_VT.value as u32,
                    )
                {
                    game.session_path(active, &mut buf);
                }
            }
            if buf[0] != 0 {
                open_path = cstr(&buf).to_vec();
                crate::log::line(&format!(
                    "map: session path {}",
                    String::from_utf8_lossy(&open_path)
                )); // :286-288
            }
            // On failure there is no new document to report: the requested path is what the
            // caller asked about (:291-295).
            let reply_path: &[u8] = if opened != 0 { &open_path } else { &path };
            write_open_result(out, opened != 0, reply_path, epoch); // :289-297
            Ok(())
        }
    }
}

/// Whether some Hammer session already holds `host` (an open map): the `map.open` session-leak
/// walk, exposed for the prefab ops (m3.18), which must not point a prefab worker at an open map.
/// An unreadable session vector is an error, never "not open".
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn path_is_open(host: &[u8]) -> Result<bool, OpErr> {
    let game: &dyn Game = &LIVE;
    let Some(app) = game.app() else {
        return Err(OpErr::new(-32003, "NoActiveDocument", "hammer_unavailable"));
    };
    match game.session_of(app, host) {
        SessionOf::Unreadable => Err(OpErr::new(-32603, "InternalError", "session_vector_unreadable")),
        SessionOf::Found { .. } => Ok(true),
        SessionOf::None => Ok(false),
    }
}

/// `op_map_open`'s process facts (pet's `open_gate`, `ops/pet/game.rs:2192-2194`): the RAW
/// exe path (`std::env::current_exe`, the same question `GetModuleFileNameA` asks) and the
/// raw command line. [`host_gate_with`] derives the app root and addon name from these
/// itself (`kv3_check::app_root_into` requires the string to end
/// `game\bin\win64\cs2.exe`); handing it the already-derived `pet_game::app_root()` /
/// `addon_name()` values would derive a second time and always refuse `no_app_root`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn raw_process_facts() -> (Option<String>, Option<String>) {
    let exe = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    (exe, Some(pet_game::command_line()))
}

/// `op_map_open` over the process's own facts (the raw exe, the command line) and the Win32
/// file probes.
#[cfg(windows)]
fn op_map_open(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    let (exe, cmdline) = raw_process_facts();
    op_map_open_gated(
        &LIVE,
        exe.as_deref(),
        cmdline.as_deref(),
        file_fact,
        header_gate,
        params,
        out,
    )
}

/// The absent-platform entry: the gates run, the file probe refuses (there is no process
/// exe/addon fact to get past them with, exactly the C's `no_app_root`), and the app resolve
/// then refuses `hammer_unavailable`. A body is never reached with hammer.dll unverified (the
/// `-32017` gate), so this is a loud last line, not a path.
#[cfg(not(windows))]
fn op_map_open(params: Option<&str>, out: &mut Sb) -> Result<(), OpErr> {
    op_map_open_gated(
        &LIVE,
        None,
        None,
        |_| HostFact::NoSuchFile,
        |_| Ok(()),
        params,
        out,
    )
}

/// The production file probe (`mcp_bridge_map.inc:97-105`): `CreateFileA` with the C's exact
/// flags, then `GetFileType`. The handle is closed before the answer crosses.
#[cfg(windows)]
pub(crate) fn file_fact(host: &[u8]) -> HostFact {
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, GetFileType, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_TYPE_DISK, OPEN_EXISTING,
    };
    let Ok(host) = std::ffi::CString::new(host.to_vec()) else {
        // The host path was `cstr`'d, so it carries no interior NUL; this cannot happen.
        return HostFact::NoSuchFile;
    };
    // SAFETY: `host` is a NUL-terminated ANSI path that outlives the call; the handle is
    // checked before use and closed on every path below.
    let handle = unsafe {
        CreateFileA(
            host.as_ptr().cast(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return HostFact::NoSuchFile; // :99-102
    }
    // SAFETY: `handle` is the live handle just opened.
    let kind = unsafe { GetFileType(handle) };
    // SAFETY: as above; both arms close it.
    unsafe { CloseHandle(handle) };
    if kind != FILE_TYPE_DISK {
        return HostFact::NotAFile; // :103-105
    }
    HostFact::Disk
}

/// The production header reader (`mcp_bridge_map.inc:129-137`): `CreateFileA` + `ReadFile`
/// into the 512-byte window; `None` = the file vanished between the gates, `Some(0)` = the
/// `ReadFile` failure (`if (!ReadFile(...)) got = 0`).
#[cfg(windows)]
fn read_header_window(host: &[u8], head: &mut [u8]) -> Option<u32> {
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let Ok(host) = std::ffi::CString::new(host.to_vec()) else {
        return None;
    };
    // SAFETY: `host` is a NUL-terminated ANSI path that outlives the call; the handle is
    // checked and closed on every path below.
    let handle = unsafe {
        CreateFileA(
            host.as_ptr().cast(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return None; // :133-136: vanished between the gates
    }
    // `ReadFile(f, head, sizeof head, &got, 0)` (:137). This windows-sys feature set has no
    // `ReadFile`, so the read goes through `std::fs::File` over the raw handle - the same
    // syscall (the pet lane's `read_vpcf` does the same; no answer changes).
    use std::os::windows::io::FromRawHandle as _;
    use std::io::Read as _;
    // SAFETY: `handle` is the live handle just opened; ownership moves to the `File`, which
    // closes it exactly once (replacing the C's `CloseHandle`).
    let mut file = unsafe { std::fs::File::from_raw_handle(handle) };
    // `if (!ReadFile(f, head, sizeof head, &got, 0)) got = 0;` (:137): a failed read is a
    // zero-byte read, the same refusal a wrong header takes.
    match file.read(head) {
        Ok(got) if got <= head.len() => Some(got as u32),
        _ => Some(0),
    }
}

/// `map_header_gate` over the Win32 reader: open the host file, read the first
/// `MAP_VMAP_HEADER_MAX` bytes, require the binary-v9 DMX header.
#[cfg(windows)]
pub(crate) fn header_gate(host: &[u8]) -> Result<(), OpErr> {
    header_gate_with(host, |head| read_header_window(host, head))
}

// -------------------------------------------------------------------------------------------
// `map.reload` / `map.save` (the arms, `mcp_bridge.c:6097-6233`)
// -------------------------------------------------------------------------------------------

/// A non-Ok arm answer: the C's `build_error_ext` arguments (`mcp_bridge.c:6102`, `:6108`,
/// `:6112`, `:6129`, `:6134`, `:6185`, `:6198`).
type ArmErr = (i64, &'static str, String);

fn doc_err(code: i64, name: &'static str, reason: &str) -> ArmErr {
    let e = OpErr::new(code, name, reason);
    (e.code, e.name, e.data)
}

/// `map.reload`'s arm (`mcp_bridge.c:6097-6165`) over a reader. The five error shapes come
/// straight from the C's `build_error_ext` calls; the success reply is [`reload_reply`].
fn reload_arm(game: &dyn Game, id: &str, params: Option<&str>) -> Result<String, ArmErr> {
    // `active_doc` (:6098-6106).
    let doc = game
        .active_doc()
        .map_err(|reason| doc_err(-32003, "NoActiveDocument", reason))?;
    // `doc_editable` (:6107-6109).
    let (editable, why) = game.doc_editable(doc);
    if !editable {
        return Err(doc_err(-32018, "DocumentNotEditable", why));
    }
    // `expect_doc_ok` (:6110-6114): the document the caller checked with doc.active is not
    // this one any more - refuse instead of reloading the other document (review L2).
    let (doc_seq, epoch_now) = hammer::fingerprint_ids();
    if let Err(why) = expect_doc_ok(params, doc_seq, epoch_now) {
        return Err(doc_err(-32004, "DocumentChanged", why));
    }
    // the doc-session reads (:6116-6119).
    let facts = game.doc_session_facts(doc);
    let dirty = game.doc_or_dependency_changed(doc); // :6120
    // `force` (:6121-6125): `json_find`'s value start, the 't' of `true`. `json_find` can
    // answer a value start at the very end of truncated params (`"force":   `); the C reads
    // the NUL there and answers force=0, so the byte read is a `get`, not an index.
    let force = match params {
        Some(params) => json_find(params.as_bytes(), b"force")
            .is_some_and(|at| params.as_bytes().get(at) == Some(&b't')),
        None => false,
    };
    // a new/untitled document has no file to reload from: the same gate, code and data text
    // `map.save` answers with, before anything is called (review M1, :6127-6132).
    if facts.is_new || facts.path_ptr == 0 || facts.path_first == 0 {
        return Err((
            -32015,
            "UntitledDocument",
            "{\"reason\":\"no path (Save-As is modal)\"}".to_string(),
        ));
    }
    if dirty && !force {
        return Err((
            -32016,
            "UnsavedChanges",
            "{\"reason\":\"document is dirty; pass force:true to discard\"}".to_string(),
        ));
    }
    // The path text is copied BEFORE ReloadFromDisk (review-5 L2, :6141-6144); the C ignores
    // md_str's answer - a failed copy leaves the empty string the reply reports.
    let mut pathbuf = [0u8; RELOAD_PATH_CAP];
    // SAFETY: the doc's path pointer, whose first byte `doc_session_facts` probed.
    unsafe { md_str(facts.path_ptr as *const u8, &mut pathbuf) };
    game.reload_from_disk(facts.session); // :6155
    hammer::doc_fingerprint(); // :6156: reload changes the fingerprint -> epoch bump
    let (_, epoch) = hammer::fingerprint_ids();
    let path: &[u8] = if pathbuf[0] != 0 { cstr(&pathbuf) } else { b"" };
    Ok(reload_reply(id, path, dirty, epoch))
}

/// `map.save`'s arm (`mcp_bridge.c:6166-6233`) over a reader. Note: NO `expect_doc_ok` and no
/// epoch member - the C's save arm has neither.
fn save_arm(game: &dyn Game, id: &str, _params: Option<&str>) -> Result<String, ArmErr> {
    let doc = game
        .active_doc()
        .map_err(|reason| doc_err(-32003, "NoActiveDocument", reason))?;
    let (editable, why) = game.doc_editable(doc);
    if !editable {
        return Err(doc_err(-32018, "DocumentNotEditable", why));
    }
    let facts = game.doc_session_facts(doc); // :6180-6183
    if facts.is_new || facts.path_ptr == 0 || facts.path_first == 0 {
        return Err((
            -32015,
            "UntitledDocument",
            "{\"reason\":\"no path (Save-As is modal)\"}".to_string(),
        ));
    }
    let dirty = game.doc_or_dependency_changed(doc); // :6187
    let mut saved = true;
    let mut skipped = true;
    if dirty {
        skipped = false;
        saved = game.save(facts.session, facts.path_ptr); // :6191
    }
    // The C hands the raw `char *` to `sb_addjson` (:6204); this port probes it (see
    // SAVE_PATH_PROBE). A failed probe answers the empty string where the C would fault.
    let mut buf = vec![0u8; SAVE_PATH_PROBE];
    // SAFETY: the doc's path pointer, whose first byte `doc_session_facts` probed.
    let read = unsafe { md_str(facts.path_ptr as *const u8, &mut buf) };
    let path: &[u8] = if read != 0 { cstr(&buf) } else { b"" };
    Ok(save_reply(id, path, saved, skipped))
}

// -------------------------------------------------------------------------------------------
// Registration
// -------------------------------------------------------------------------------------------

/// The C's dispatch-chain order (`mcp_bridge.c:6083`, `:6097`, `:6166`).
pub static HANDLERS: &[(&str, crate::ops::Handler)] = &[
    ("map.open", map_open),
    ("map.reload", map_reload),
    ("map.save", map_save),
];

/// `map.open`'s arm tail (`mcp_bridge.c:6083-6096`): the 2048-byte result buffer, the C's
/// overflow message.
fn map_open(req: &Request) -> String {
    let mut buf = vec![0u8; OPEN_RESULT_CAP];
    let mut out = Sb::new(&mut buf);
    let outcome = op_map_open(req.params.as_deref(), &mut out);
    finish(
        &req.id,
        outcome,
        &out,
        "InternalError: map.open result too large", // :6091
    )
}

/// `map.reload`'s arm: the errors are the C's inline `build_error_ext` calls, the success the
/// inline envelope.
fn map_reload(req: &Request) -> String {
    match reload_arm(&LIVE, &req.id, req.params.as_deref()) {
        Ok(line) => line,
        Err((code, name, data)) => control::error_ext(Some(&req.id), code, name, &data),
    }
}

/// `map.save`'s arm, same shape as [`map_reload`].
fn map_save(req: &Request) -> String {
    match save_arm(&LIVE, &req.id, req.params.as_deref()) {
        Ok(line) => line,
        Err((code, name, data)) => control::error_ext(Some(&req.id), code, name, &data),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        expect_doc_ok, header_gate_with, host_gate_with, md_str, params_gate, reload_arm,
        save_arm, write_existing_result, write_open_result, ArmErr, HostFact, SessionOf,
    };
    use crate::entity_json::Sb;
    use crate::ops::hammer::{self, Doc};
    use crate::ops::map::{DocSessionFacts, Game};
    use crate::ops::pet::game as pet_game;
    use crate::ops::OpErr;
    use std::sync::Mutex;

    /// The C's `BRIDGE_BUILD` (`mcp_bridge.c:127`), spelled as every reply spells it.
    const BUILD: &str = crate::BRIDGE_BUILD;

    /// A process-fact pair that gets past `app_root_into` / `addon_name_into`
    /// (`kv3_check.rs:785`, `:706`): the exe must end `game\bin\win64\cs2.exe`.
    const EXE: &str = "Z:\\app\\game\\bin\\win64\\cs2.exe";
    /// The command line `addon_name_into` parses (`kv3_check.rs:706`).
    const CMDLINE: &str = "-addon addon1";
    /// The host path those two facts derive for `maps/x.vmap`
    /// (`kv3_check::host_path_into`, `<app>\content\csgo_addons\<addon>\<rel>`).
    fn host() -> Vec<u8> {
        crate::kv3_check::host_path("Z:\\app", "addon1", "maps/x.vmap")
            .expect("the shape app_root_into accepts derives a host path")
            .into_bytes()
    }

    /// The result builder's bytes (the C's `rb.buf` up to its NUL).
    fn build<F: FnOnce(&mut Sb)>(cap: usize, f: F) -> Vec<u8> {
        let mut buf = vec![0u8; cap];
        let mut out = Sb::new(&mut buf);
        f(&mut out);
        super::cstr(out.as_cstr()).to_vec()
    }

    // ---------------------------------------------------------------------------------------
    // The result builders, byte for byte over the C's `sb_add` sequence
    // ---------------------------------------------------------------------------------------

    /// `mcp_bridge_map.inc:252-255`:
    /// `{"opened":false,"existing":true,"path":` addjson `,"active":` `,"epoch":` addnum `}`.
    #[test]
    fn existing_result_bytes() {
        assert_eq!(
            build(256, |out| write_existing_result(
                out,
                b"Z:\\maps\\x.vmap",
                true,
                7,
            )),
            &br#"{"opened":false,"existing":true,"path":"Z:\\maps\\x.vmap","active":true,"epoch":7}"#[..]
        );
        assert_eq!(
            build(256, |out| write_existing_result(out, b"maps/x.vmap", false, 0)),
            &br#"{"opened":false,"existing":true,"path":"maps/x.vmap","active":false,"epoch":0}"#[..]
        );
    }

    /// `mcp_bridge_map.inc:289-297`:
    /// `{"opened":` `,"existing":false,"path":` addjson [`,"reason":"open_failed"`] `,"epoch":` `}`.
    #[test]
    fn open_result_bytes() {
        assert_eq!(
            build(256, |out| write_open_result(
                out,
                true,
                b"Z:\\maps\\x.vmap",
                7,
            )),
            &br#"{"opened":true,"existing":false,"path":"Z:\\maps\\x.vmap","epoch":7}"#[..]
        );
        assert_eq!(
            build(256, |out| write_open_result(out, false, b"maps/x.vmap", 7)),
            &br#"{"opened":false,"existing":false,"path":"maps/x.vmap","reason":"open_failed","epoch":7}"#[..]
        );
    }

    /// `mcp_bridge.c:6157-6165`: the inline envelope, `"result":{"reloaded":true,"path":`
    /// addjson `,"discardedChanges":` `,"epoch":` addnum `}}\n`.
    #[test]
    fn reload_reply_bytes() {
        assert_eq!(
            super::reload_reply("i1", b"Z:\\maps\\x.vmap", true, 9),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i1\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"reloaded\":true,\"path\":\"Z:\\\\maps\\\\x.vmap\",\"discardedChanges\":true,\"epoch\":9}}}}\n"
            )
        );
        // A failed pre-reload copy reports the empty string (`pathbuf[0] ? pathbuf : ""`).
        assert_eq!(
            super::reload_reply("i", b"", false, 0),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"reloaded\":true,\"path\":\"\",\"discardedChanges\":false,\"epoch\":0}}}}\n"
            )
        );
    }

    /// `mcp_bridge.c:6199-6206`: `"result":{"saved":` `,"path":` addjson
    /// `,"skippedClean":` `}}\n`.
    #[test]
    fn save_reply_bytes() {
        assert_eq!(
            super::save_reply("i2", b"Z:\\maps\\x.vmap", true, false),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i2\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"saved\":true,\"path\":\"Z:\\\\maps\\\\x.vmap\",\"skippedClean\":false}}}}\n"
            )
        );
        assert_eq!(
            super::save_reply("i", b"maps/x.vmap", false, true),
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"saved\":false,\"path\":\"maps/x.vmap\",\"skippedClean\":true}}}}\n"
            )
        );
    }

    // ---------------------------------------------------------------------------------------
    // The parameter gates (`op_map_open`'s head)
    // ---------------------------------------------------------------------------------------

    /// The shared `-32602` data of `!params` / `!json_string` / `!path[0]`
    /// (`mcp_bridge_map.inc:227-233`).
    #[test]
    fn missing_or_empty_path_refusals() {
        let expected = OpErr::new(
            -32602,
            "InvalidParams",
            "path: relative to the addon content root (maps/... .vmap), 1..512 chars, no '..'",
        );
        assert_eq!(params_gate(None).unwrap_err(), expected);
        assert_eq!(params_gate(Some("{}")).unwrap_err(), expected);
        assert_eq!(params_gate(Some(r#"{"path":""}"#)).unwrap_err(), expected);
        assert_eq!(params_gate(Some(r#"{"path":123}"#)).unwrap_err(), expected);
        assert_eq!(
            params_gate(Some(r#"{"path":"maps/no.vmd"}"#)).unwrap_err(),
            OpErr::new(-32602, "InvalidParams", "path: a .vmap file")
        );
    }

    /// A `\` byte in the RAW token refuses before the shape gate
    /// (`mcp_bridge_map.inc:235-238`).
    #[test]
    fn escapes_are_refused_on_the_raw_token() {
        assert_eq!(
            params_gate(Some(r#"{"path":"maps\\x.vmap"}"#)).unwrap_err(),
            OpErr::new(
                -32602,
                "InvalidParams",
                "path: no JSON escapes (use '/' separators)"
            )
        );
        assert_eq!(
            params_gate(Some(r#"{"path":"maps\/x.vmap"}"#)).unwrap_err(),
            OpErr::new(
                -32602,
                "InvalidParams",
                "path: no JSON escapes (use '/' separators)"
            )
        );
        // An escape OUTSIDE the path member does not refuse: the scan runs on `json_find`'s
        // value only (`:229`).
        assert_eq!(
            params_gate(Some(r#"{"note":"a\\b","path":"maps/x.vmap"}"#)).unwrap(),
            b"maps/x.vmap".to_vec()
        );
    }

    /// The shape gate's reason is the C header's own string (`map_path.h` via
    /// `map_path::Reason::as_str`).
    #[test]
    fn shape_refusals_are_the_c_reasons() {
        assert_eq!(
            params_gate(Some(r#"{"path":"maps/../x.vmap"}"#)).unwrap_err(),
            OpErr::new(-32602, "InvalidParams", "path: no '..'")
        );
        assert_eq!(
            params_gate(Some(r#"{"path":"maps//x.vmap"}"#)).unwrap_err(),
            OpErr::new(-32602, "InvalidParams", "path: no empty path segment ('//')")
        );
        let long = format!(r#"{{"path":"maps/{}.vmap"}}"#, "x".repeat(513));
        assert_eq!(
            params_gate(Some(&long)).unwrap_err(),
            OpErr::new(-32602, "InvalidParams", "path: 1..512 chars")
        );
        assert_eq!(
            params_gate(Some(r#"{"path":"maps/x.vmap"}"#)).unwrap(),
            b"maps/x.vmap".to_vec()
        );
    }

    // ---------------------------------------------------------------------------------------
    // The host / header file gates over injected facts
    // ---------------------------------------------------------------------------------------

    #[test]
    fn host_gate_refusals() {
        assert_eq!(
            host_gate_with(None, Some(CMDLINE), b"maps/x.vmap", |_| HostFact::Disk)
                .unwrap_err(),
            OpErr::new(-32017, "PrerequisiteFailed", "no_app_root")
        );
        assert_eq!(
            host_gate_with(Some("C:\\x"), Some(CMDLINE), b"maps/x.vmap", |_| HostFact::Disk)
                .unwrap_err(),
            OpErr::new(-32017, "PrerequisiteFailed", "no_app_root")
        );
        assert_eq!(
            host_gate_with(Some(EXE), None, b"maps/x.vmap", |_| HostFact::Disk).unwrap_err(),
            OpErr::new(-32017, "PrerequisiteFailed", "no_addon")
        );
        assert_eq!(
            host_gate_with(Some(EXE), Some("-addon"), b"maps/x.vmap", |_| HostFact::Disk)
                .unwrap_err(),
            OpErr::new(-32017, "PrerequisiteFailed", "no_addon")
        );
        assert_eq!(
            host_gate_with(Some(EXE), Some(CMDLINE), b"maps/x.vmap", |_| HostFact::NoSuchFile)
                .unwrap_err(),
            OpErr::new(-32014, "AssetNotFound", "no_such_file")
        );
        assert_eq!(
            host_gate_with(Some(EXE), Some(CMDLINE), b"maps/x.vmap", |_| HostFact::NotAFile)
                .unwrap_err(),
            OpErr::new(-32014, "AssetNotFound", "not_a_file")
        );
    }

    /// The derived host path is `<app>\content\csgo_addons\<addon>\<rel>` with `/` folded
    /// (`kv3_check::host_path_into`).
    #[test]
    fn host_gate_derives_the_host_path() {
        assert_eq!(
            host_gate_with(Some(EXE), Some(CMDLINE), b"maps/x.vmap", |host| {
                assert_eq!(
                    host,
                    b"Z:\\app\\content\\csgo_addons\\addon1\\maps\\x.vmap",
                    "the probe receives exactly the derived host path"
                );
                HostFact::Disk
            })
            .unwrap(),
            host()
        );
    }

    /// Review R2-1: the production argument shapes get past the gate, the derived ones do
    /// not. `op_map_open` hands [`host_gate_with`] the RAW exe path and command line (pet's
    /// `open_gate`, `ops/pet/game.rs:2192-2194`), which `app_root_into` / `addon_name_into`
    /// derive themselves; feeding the already-derived `pet_game::app_root()` (`<app>`,
    /// which no longer ends `game\bin\win64\cs2.exe`) back in derives a second time and
    /// always refused `-32017 no_app_root`.
    #[test]
    fn open_wiring_passes_raw_process_facts() {
        // The raw shapes the live path passes: EXE ends `game\bin\win64\cs2.exe`, CMDLINE
        // is `-addon addon1` - the gate derives and reaches the file probe.
        assert_eq!(
            host_gate_with(Some(EXE), Some(CMDLINE), b"maps/x.vmap", |_| HostFact::Disk)
                .unwrap(),
            host()
        );

        // The pre-derived app root (what `pet_game::app_root()` returns for EXE) is
        // refused again: the regression the raw wiring removed.
        let mut app = [0u8; pet_game::APP_CAP];
        assert!(crate::kv3_check::app_root_into(EXE, &mut app));
        let derived = String::from_utf8(super::cstr(&app).to_vec()).unwrap();
        assert_eq!(
            host_gate_with(
                Some(derived.as_str()),
                Some(CMDLINE),
                b"maps/x.vmap",
                |_| HostFact::Disk
            )
            .unwrap_err(),
            OpErr::new(-32017, "PrerequisiteFailed", "no_app_root")
        );

        // The factored wiring forwards the process's own RAW facts, not the derived pair:
        // on this host the derived values are None (nothing ends `cs2.exe`, no `-addon`),
        // while the raw exe path and command line are Some.
        let (exe, cmdline) = super::raw_process_facts();
        assert_eq!(
            exe.as_deref(),
            std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
            "the exe is forwarded raw, not the derived app root"
        );
        let expected_cmdline = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            cmdline.as_deref(),
            Some(expected_cmdline.as_str()),
            "the command line is forwarded raw, not the parsed addon name"
        );
    }

    #[test]
    fn header_gate_refusals() {
        let host = host();
        assert_eq!(
            header_gate_with(&host, |_| None).unwrap_err(),
            OpErr::new(-32014, "AssetNotFound", "no_such_file")
        );
        assert_eq!(
            header_gate_with(&host, |_| Some(0)).unwrap_err(),
            OpErr::new(-32014, "AssetNotFound", "InvalidMapFile")
        );
        // A `binary 8` header shares the 16-byte prefix with the accepted one (review H1).
        let bin8 = b"<!-- dmx encoding binary 8 format maps 1 -->\0";
        assert_eq!(
            header_gate_with(&host, |out| {
                out[..bin8.len()].copy_from_slice(bin8);
                Some(bin8.len() as u32)
            })
            .unwrap_err(),
            OpErr::new(-32014, "AssetNotFound", "InvalidMapFile")
        );
        assert!(header_gate_with(&host, |out| {
            let good = b"<!-- dmx encoding binary 9 format maps 1 -->\0";
            out[..good.len()].copy_from_slice(good);
            Some(good.len() as u32)
        })
        .is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // `expect_doc_ok` (`mcp_bridge.c:1920-1938`)
    // ---------------------------------------------------------------------------------------

    #[test]
    fn expect_doc_refusals() {
        assert_eq!(expect_doc_ok(None, 3, 5).unwrap_err(), "missing");
        assert_eq!(expect_doc_ok(Some("{}"), 3, 5).unwrap_err(), "missing");
        assert_eq!(
            expect_doc_ok(Some(r#"{"expectDoc":3}"#), 3, 5).unwrap_err(),
            "missing"
        );
        assert_eq!(
            expect_doc_ok(Some(r#"{"expectDoc":{"epoch":5}}"#), 3, 5).unwrap_err(),
            "missing_id"
        );
        assert_eq!(
            expect_doc_ok(Some(r#"{"expectDoc":{"id":"d3"}}"#), 3, 5).unwrap_err(),
            "missing_epoch"
        );
        assert_eq!(
            expect_doc_ok(Some(r#"{"expectDoc":{"id":"d4","epoch":5}}"#), 3, 5)
                .unwrap_err(),
            "doc_id"
        );
        assert_eq!(
            expect_doc_ok(Some(r#"{"expectDoc":{"id":"d3","epoch":6}}"#), 3, 5)
                .unwrap_err(),
            "epoch"
        );
        assert!(expect_doc_ok(Some(r#"{"expectDoc":{"id":"d3","epoch":5}}"#), 3, 5).is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // `md_str` (`mcp_bridge.c:5169-5186`)
    // ---------------------------------------------------------------------------------------

    #[test]
    fn md_str_copies_cuts_and_refuses() {
        let storage = b"Z:\\maps\\x.vmap\0".to_vec();
        // SAFETY: `storage` is live process memory the probe may read.
        assert_eq!(unsafe { md_str(storage.as_ptr(), &mut [0u8; 64]) }, 1);
        // A cut at cap-1 answers -1 and keeps a raw (possibly mid-codepoint) prefix - the
        // difference from the hammer lane's probe_str (`:5185`).
        let mut out = [0u8; 8];
        // SAFETY: as above.
        assert_eq!(unsafe { md_str(storage.as_ptr(), &mut out) }, -1);
        assert_eq!(&out[..7], b"Z:\\maps");
        assert_eq!(out[7], 0);
        // A null pointer answers 0 with `out[0] == 0`.
        let mut out = [0u8; 8];
        // SAFETY: a null pointer is refused inside.
        assert_eq!(unsafe { md_str(core::ptr::null(), &mut out) }, 0);
        assert_eq!(out[0], 0);
    }

    // ---------------------------------------------------------------------------------------
    // The arms over a fake reader - every fake answer cites the C line it mirrors, and the
    // fake asserts the arguments it receives
    // ---------------------------------------------------------------------------------------

    struct Fake {
        app: Option<usize>,
        doc: Result<Doc, &'static str>,
        editable: (bool, &'static str),
        sessions: SessionOf,
        /// session address -> its path text (`map_session_path`'s 1).
        paths: Vec<(usize, Vec<u8>)>,
        active: usize,
        facts: DocSessionFacts,
        dirty: i32,
        save_result: bool,
        open_result: usize,
        open_calls: Mutex<Vec<(usize, Vec<u8>)>>,
        reload_calls: Mutex<Vec<usize>>,
        save_calls: Mutex<Vec<(usize, usize)>>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                app: Some(0x10_00_00),
                doc: Ok(Doc(0x50_00)),
                editable: (true, "ok"),
                sessions: SessionOf::None,
                paths: Vec::new(),
                active: 0,
                // `mcp_bridge.c:6116-6119` with a live owner session (0x6000), not new.
                facts: DocSessionFacts {
                    session: 0x60_00,
                    is_new: false,
                    path_ptr: 0,
                    path_first: 0,
                },
                dirty: 0,
                save_result: true,
                open_result: 0,
                open_calls: Mutex::new(Vec::new()),
                reload_calls: Mutex::new(Vec::new()),
                save_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Fake {
        /// Point `facts.path_ptr` at `storage` (a NUL-terminated path kept alive by the caller).
        fn with_path_storage(&mut self, storage: &[u8]) {
            self.facts.path_ptr = storage.as_ptr() as usize;
            self.facts.path_first = storage[0];
        }
    }

    impl Game for Fake {
        fn app(&self) -> Option<usize> {
            self.app
        }
        fn active_doc(&self) -> Result<Doc, &'static str> {
            self.doc
        }
        fn doc_editable(&self, _doc: Doc) -> (bool, &'static str) {
            self.editable
        }
        fn session_of(&self, app: usize, _host: &[u8]) -> SessionOf {
            assert_eq!(Some(app), self.app, "session_of receives the resolved app");
            self.sessions.clone()
        }
        fn session_path(&self, session: usize, out: &mut [u8]) -> bool {
            match self.paths.iter().find(|(at, _)| *at == session) {
                Some((_, text)) => {
                    out[..text.len()].copy_from_slice(text);
                    out[text.len()] = 0;
                    true
                }
                None => false,
            }
        }
        fn active_session(&self, _app: usize) -> usize {
            self.active
        }
        fn doc_session_facts(&self, doc: Doc) -> DocSessionFacts {
            assert_eq!(Ok(doc), self.doc, "the facts are read for the resolved doc");
            self.facts
        }
        fn doc_or_dependency_changed(&self, doc: Doc) -> bool {
            assert_eq!(Ok(doc), self.doc, "the dirty check is for the resolved doc");
            self.dirty != 0
        }
        fn reload_from_disk(&self, session: usize) {
            self.reload_calls.lock().unwrap().push(session);
        }
        fn save(&self, session: usize, path_ptr: usize) -> bool {
            self.save_calls.lock().unwrap().push((session, path_ptr));
            self.save_result
        }
        fn open_file(&self, app: usize, path: &[u8]) -> usize {
            self.open_calls.lock().unwrap().push((app, path.to_vec()));
            self.open_result
        }
    }

    fn run_open(fake: &Fake, params: Option<&str>) -> Result<Vec<u8>, OpErr> {
        let mut buf = vec![0u8; super::OPEN_RESULT_CAP];
        let mut out = Sb::new(&mut buf);
        super::op_map_open_gated(
            fake,
            Some(EXE),
            Some(CMDLINE),
            |_| HostFact::Disk,
            |host| {
                header_gate_with(host, |out| {
                    let good = b"<!-- dmx encoding binary 9 format maps 1 -->\0";
                    out[..good.len()].copy_from_slice(good);
                    Some(good.len() as u32)
                })
            },
            params,
            &mut out,
        )
        .map(|()| super::cstr(out.as_cstr()).to_vec())
    }

    /// The C's inline `build_error_ext` shapes, as the arms answer them.
    fn assert_arm_err(got: ArmErr, code: i64, name: &str, data: &str) {
        assert_eq!(got, (code, name, data.to_string()));
    }

    #[test]
    fn open_without_the_app_refuses() {
        let fake = Fake {
            app: None,
            ..Default::default()
        };
        assert_eq!(
            run_open(&fake, Some(r#"{"path":"maps/x.vmap"}"#)).unwrap_err(),
            OpErr::new(-32003, "NoActiveDocument", "hammer_unavailable")
        );
    }

    #[test]
    fn open_with_an_unreadable_session_vector_refuses() {
        let fake = Fake {
            sessions: SessionOf::Unreadable,
            ..Default::default()
        };
        assert_eq!(
            run_open(&fake, Some(r#"{"path":"maps/x.vmap"}"#)).unwrap_err(),
            OpErr::new(-32603, "InternalError", "session_vector_unreadable")
        );
    }

    /// An already-open file answers `existing:true` and NEVER calls OpenFile
    /// (`mcp_bridge_map.inc:243-256`).
    #[test]
    fn open_of_an_existing_session_does_not_call_openfile() {
        let host = host();
        // `addjson`'s escaping of the host path (backslashes doubled).
        let path_json =
            br#""Z:\\app\\content\\csgo_addons\\addon1\\maps\\x.vmap""#.to_vec();
        let mut fake = Fake {
            sessions: SessionOf::Found {
                session: 0x70_00,
                path: host.clone(),
                active: true,
            },
            ..Default::default()
        };
        fake.paths.push((0x70_00, host));
        let out = run_open(&fake, Some(r#"{"path":"maps/x.vmap"}"#)).unwrap();
        assert_eq!(
            out,
            format!(
                r#"{{"opened":false,"existing":true,"path":{},"active":true,"epoch":{}}}"#,
                String::from_utf8_lossy(&path_json),
                hammer::fingerprint_ids().1,
            )
            .into_bytes()
        );
        assert!(fake.open_calls.lock().unwrap().is_empty());
    }

    /// `OpenFile` is called with the requested ADDON-RELATIVE path (`:268`), and the session
    /// it returns answers the reply's path (`:273-297`).
    #[test]
    fn open_calls_openfile_with_the_relative_path() {
        let mut fake = Fake {
            open_result: 0x80_00,
            ..Default::default()
        };
        fake.paths.push((0x80_00, b"Z:\\win-path\\x.vmap".to_vec()));
        let out = run_open(&fake, Some(r#"{"path":"maps/x.vmap"}"#)).unwrap();
        assert_eq!(
            fake.open_calls.lock().unwrap().as_slice(),
            [(fake.app.unwrap(), b"maps/x.vmap".to_vec())]
        );
        assert_eq!(
            out,
            format!(
                r#"{{"opened":true,"existing":false,"path":"Z:\\win-path\\x.vmap","epoch":{}}}"#,
                hammer::fingerprint_ids().1,
            )
            .into_bytes()
        );
    }

    /// `opened:false` reports the requested path + `open_failed` (`:289-295`).
    #[test]
    fn open_failure_reports_the_requested_path() {
        let fake = Fake {
            open_result: 0,
            ..Default::default()
        };
        let out = run_open(&fake, Some(r#"{"path":"maps/x.vmap"}"#)).unwrap();
        assert_eq!(
            out,
            format!(
                r#"{{"opened":false,"existing":false,"path":"maps/x.vmap","reason":"open_failed","epoch":{}}}"#,
                hammer::fingerprint_ids().1,
            )
            .into_bytes()
        );
    }

    /// The reload arm's refusals, in the C's order (`mcp_bridge.c:6098-6136`).
    #[test]
    fn reload_refusals_in_order() {
        let (doc_seq, epoch) = hammer::fingerprint_ids();
        let expect = format!(r#"{{"expectDoc":{{"id":"d{doc_seq}","epoch":{epoch}}}}}"#);

        let fake = Fake {
            doc: Err("no_session"),
            ..Default::default()
        };
        assert_arm_err(
            reload_arm(&fake, "i", None).unwrap_err(),
            -32003,
            "NoActiveDocument",
            r#"{"reason":"no_session"}"#,
        );

        let fake = Fake {
            editable: (false, "edit_scope_active"),
            ..Default::default()
        };
        assert_arm_err(
            reload_arm(&fake, "i", Some(&expect)).unwrap_err(),
            -32018,
            "DocumentNotEditable",
            r#"{"reason":"edit_scope_active"}"#,
        );

        let fake = Fake::default();
        assert_arm_err(
            reload_arm(&fake, "i", None).unwrap_err(),
            -32004,
            "DocumentChanged",
            r#"{"reason":"missing"}"#,
        );
        assert_arm_err(
            reload_arm(
                &fake,
                "i",
                Some(r#"{"expectDoc":{"id":"d999999","epoch":999999}}"#),
            )
            .unwrap_err(),
            -32004,
            "DocumentChanged",
            r#"{"reason":"doc_id"}"#,
        );

        // A new/untitled document refuses with map.save's data text, before anything is
        // called (review M1, `mcp_bridge.c:6127-6132`).
        let mut fake = Fake::default();
        fake.facts.is_new = true;
        fake.with_path_storage(b"Z:\\x\0");
        assert_arm_err(
            reload_arm(&fake, "i", Some(&expect)).unwrap_err(),
            -32015,
            "UntitledDocument",
            r#"{"reason":"no path (Save-As is modal)"}"#,
        );

        // Dirty without force refuses (`:6133-6136`).
        let mut fake = Fake::default();
        let dirty_storage = b"Z:\\win\\maps\\x.vmap\0".to_vec();
        fake.with_path_storage(&dirty_storage);
        fake.dirty = 1;
        assert_arm_err(
            reload_arm(&fake, "i", Some(&expect)).unwrap_err(),
            -32016,
            "UnsavedChanges",
            r#"{"reason":"document is dirty; pass force:true to discard"}"#,
        );
    }

    /// Dirty with force reloads; the reply is the C's inline envelope
    /// (`mcp_bridge.c:6137-6165`).
    #[test]
    fn reload_with_force_calls_reloadfromdisk() {
        let (doc_seq, epoch) = hammer::fingerprint_ids();
        let storage = b"Z:\\win\\maps\\x.vmap\0".to_vec();
        let mut fake = Fake {
            dirty: 1,
            ..Default::default()
        };
        fake.with_path_storage(&storage);
        let with_force = format!(
            r#"{{"expectDoc":{{"id":"d{doc_seq}","epoch":{epoch}}},"force":true}}"#
        );
        let reply = reload_arm(&fake, "i", Some(&with_force)).unwrap();
        assert_eq!(
            fake.reload_calls.lock().unwrap().as_slice(),
            [0x60_00],
            "ReloadFromDisk receives the owner session"
        );
        assert_eq!(
            reply,
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"reloaded\":true,\"path\":\"Z:\\\\win\\\\maps\\\\x.vmap\",\"discardedChanges\":true,\"epoch\":{}}}}}\n",
                hammer::fingerprint_ids().1,
            )
        );
    }

    /// Review R2-3: `json_find` can answer a value start at the very end of truncated
    /// params (`"force":   `, nothing after the blanks) - the C reads the NUL there and
    /// answers force=0 (`mcp_bridge.c:6121-6125`), so a dirty document still refuses
    /// `UnsavedChanges` instead of the byte read panicking.
    #[test]
    fn reload_force_truncated_reads_the_nul() {
        let (doc_seq, epoch) = hammer::fingerprint_ids();
        let storage = b"Z:\\win\\maps\\x.vmap\0".to_vec();
        let mut fake = Fake {
            dirty: 1,
            ..Default::default()
        };
        fake.with_path_storage(&storage);
        // params end right after the blanks: json_find's value start == the params length.
        let truncated =
            format!(r#"{{"expectDoc":{{"id":"d{doc_seq}","epoch":{epoch}}},"force":   "#);
        assert_arm_err(
            reload_arm(&fake, "i", Some(&truncated)).unwrap_err(),
            -32016,
            "UnsavedChanges",
            r#"{"reason":"document is dirty; pass force:true to discard"}"#,
        );
        assert!(
            fake.reload_calls.lock().unwrap().is_empty(),
            "nothing was reloaded"
        );
    }

    /// The save arm: clean skips, dirty saves, with the C's shapes
    /// (`mcp_bridge.c:6166-6233`).
    #[test]
    fn save_arm_answers() {
        let (doc_seq, epoch) = hammer::fingerprint_ids();
        let expect = format!(r#"{{"expectDoc":{{"id":"d{doc_seq}","epoch":{epoch}}}}}"#);

        let storage = b"Z:\\win\\maps\\x.vmap\0".to_vec();
        // Clean: `skippedClean:true`, Save NOT called (`:6187-6194`).
        let mut fake = Fake::default();
        fake.with_path_storage(&storage);
        let reply = save_arm(&fake, "i", Some(&expect)).unwrap();
        assert!(fake.save_calls.lock().unwrap().is_empty());
        assert_eq!(
            reply,
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"saved\":true,\"path\":\"Z:\\\\win\\\\maps\\\\x.vmap\",\"skippedClean\":true}}}}\n"
            )
        );
        // Dirty: Save(session, pathp, pathp, 0, 0) and `skippedClean:false` (`:6190-6194`).
        let mut fake = Fake {
            dirty: 1,
            ..Default::default()
        };
        fake.with_path_storage(&storage);
        let reply = save_arm(&fake, "i", Some(&expect)).unwrap();
        assert_eq!(
            fake.save_calls.lock().unwrap().as_slice(),
            [(0x60_00, storage.as_ptr() as usize)],
            "Save receives the owner session and the doc's own path pointer"
        );
        assert_eq!(
            reply,
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"i\",\"bridge\":{{\"protocol\":1,\"build\":\"{BUILD}\"}},\"result\":{{\"saved\":true,\"path\":\"Z:\\\\win\\\\maps\\\\x.vmap\",\"skippedClean\":false}}}}\n"
            )
        );
        // Dirty and the Save slot answers 0: `saved:false` (`:6192`).
        let mut fake = Fake {
            dirty: 1,
            save_result: false,
            ..Default::default()
        };
        fake.with_path_storage(&storage);
        let reply = save_arm(&fake, "i", Some(&expect)).unwrap();
        assert!(reply.contains(r#""saved":false"#) && reply.contains(r#""skippedClean":false"#));
        // Untitled refuses with the same data text reload answers (`:6185`).
        let mut fake = Fake::default();
        fake.facts.is_new = true;
        assert_arm_err(
            save_arm(&fake, "i", Some(&expect)).unwrap_err(),
            -32015,
            "UntitledDocument",
            r#"{"reason":"no path (Save-As is modal)"}"#,
        );
    }
}
