//! `McpBridge_001`: the bridge's own interface, the object `CreateInterface` hands a caller
//! that wants to ask the bridge what it is.
//!
//! **Provisional.** This interface is ours, not Valve's and not Metamod's: no header upstream
//! declares it, so nothing outside this crate may assume it is stable. It exists so a probe (a
//! test, the Wine tools, another plugin) can ask "is Metamod here, is KHook installed, how many
//! frames has the tick counted" without reading the bridge's log file. The version number
//! [`VERSION`] is bumped whenever the object's layout or the status line's shape changes, and a
//! caller must check it before calling anything.
//!
//! The shape is the C++-style one both ABIs agree on: a pointer to a vtable first, then the
//! object's own data. A C++ caller would declare
//!
//! ```cpp
//! class McpBridge {
//! public:
//!     virtual size_t GetStatus(char *buf, size_t len);
//!     uint32_t version;
//! };
//! ```
//!
//! and the Itanium/MSVC layout of that class is exactly [`McpBridge`] plus [`McpBridge_vtable`]
//! (one virtual, no destructor declared, so no dtor slot in either ABI). The vtable is a plain
//! `#[repr(C)]` struct here rather than a generated `_vtable`: the class is this crate's own,
//! so there is no upstream declaration for the generator to read.

use core::ffi::c_void;
use core::ptr;

use crate::{plugin, toolsys};

/// The interface version string `CreateInterface` serves this object under.
pub const IFACE_NAME: &core::ffi::CStr = c"McpBridge_001";

/// The object's `version` field: `3` for the layout and status line in this file (see the
/// module docs).
///
/// v2 replaced the status line's `"image":"tool"|"metamod"|"none"` with `"roles":[...]`: one
/// DLL image can serve the Metamod plugin and the tools framework at once (the Wine tools' vdf
/// points at the tool DLL), so the image has a role *set*, not one name.
///
/// v3 **adds** the tick source: `"tickSource":"tools"|"gameframe"|"none"`, the GameFrame
/// counter `"gameframeTicks"`, the hook's state `"gameframe"` (`"installed"`, or why it is
/// not - `crate::gameframe::state`), and `"gameframeOffThread"` (a frame arrived on a thread
/// other than `Load`'s, so that source is not producing ticks - `crate::gameframe::off_thread`).
/// Every v2 member keeps its name, its position before the new ones and its meaning; the object
/// layout did not change. A v2 caller that checked the version refuses this one, which is what
/// the version number is for.
pub const VERSION: u32 = 3;

/// The object's vtable: one `extern "C"` entry today, with the `this` every C++ vtable call
/// passes first (a caller reaching this table through `McpBridge`'s first word is calling a
/// virtual, so the object pointer arrives in the first argument register - a slot without it
/// would read the object as `buf`).
///
/// `GetStatus(this, buf, len)` writes the status line - a single JSON object,
/// `{"metamod":"present"|"absent","khook":bool,"tick":N,"roles":["mm","tool"],
/// "tickSource":"tools"|"gameframe"|"none","gameframeTicks":N,"gameframe":"...",
/// "gameframeOffThread":bool}` - as
/// a NUL-terminated string into
/// `buf`, at most `len` bytes including the terminator, and returns the length of the whole
/// line in bytes *excluding* the terminator (the `snprintf` rule: a return of `len` or more
/// means the buffer was too small and the line was truncated).
#[repr(C)]
#[allow(non_snake_case)] // The generated `*_vtable` structs name their fields after the C++
                           // virtuals (`GetApiVersion`, `Load`, ...); this one matches them.
pub struct McpBridge_vtable {
    /// `size_t GetStatus(McpBridge *this, uint8_t *buf, size_t len)`, the vtable entry of the
    /// C++ class below. A C++ consumer may declare the buffer `char *`: the two are the same
    /// byte type on both targets, and this interface is ours.
    pub GetStatus: unsafe extern "C" fn(this: *mut McpBridge, buf: *mut u8, len: usize) -> usize,
}

/// The object: a vtable pointer, the version, and (as in the C bridge's objects) a reserved
/// word that keeps the object 16 bytes - the size the C++ class in the module docs has on both
/// x86_64 ABIs.
#[repr(C)]
pub struct McpBridge {
    /// The vtable, as a C++ caller reads it: [`VTABLE`]'s address.
    pub vt: *const McpBridge_vtable,
    /// [`VERSION`].
    pub version: u32,
    /// Zero; reserved so the object is a round 16 bytes and a later field can be added without
    /// moving `version`.
    pub reserved: u32,
}

/// The vtable, immutable after the compile-time build.
static VTABLE: McpBridge_vtable = McpBridge_vtable {
    GetStatus: get_status,
};

/// The object. `McpBridge` is `!Sync` only because it holds a raw pointer to an immutable
/// static; nothing writes it after the `const` initialiser.
struct ObjCell(McpBridge);

// SAFETY: the object is never written after the static initialiser - `vt` points at the
// read-only `VTABLE`, and `version`/`reserved` are read-only - so sharing it is sound.
unsafe impl Sync for ObjCell {}

static OBJ: ObjCell = ObjCell(McpBridge {
    vt: &VTABLE,
    version: VERSION,
    reserved: 0,
});

/// The object's address, as `CreateInterface` returns it.
#[must_use]
pub fn object() -> *mut c_void {
    core::ptr::addr_of!(OBJ.0).cast_mut().cast::<c_void>()
}

/// The vtable the object points at.
#[must_use]
pub fn vtable() -> *const McpBridge_vtable {
    &VTABLE
}

/// The status line: one JSON object, no trailing newline.
///
/// * `"metamod"`: `"present"` once Metamod's `Load` has completed in **this image** and until
///   a successful `Unload`; `"absent"` otherwise (the `-tools` run, where there is no Metamod
///   at all).
/// * `"khook"`: whether a KHook API is installed in this process (`s2sdk::hook::api()`).
/// * `"tick"`: the tool plugin's frame counter (`toolsys::ticks()`), unchanged from v2: the
///   tools framework's `RunFrame` is the only writer, so it stays at 0 on a host without the
///   tools framework. `"tickSource"` says which entry point drives `wiring::tick` in this image
///   (`"tools"` - the tool role is present; `"gameframe"` - the Metamod-only tick,
///   `crate::gameframe`; `"none"`), `"gameframeTicks"` and `"gameframe"` report that hook's
///   counter and state (the state text is the reason when it is not installed, so a `"none"`
///   source always says why), and `"gameframeOffThread"` says the source is not producing ticks
///   because every frame arrived on a non-`Load` thread (the off-thread line was logged once).
/// * `"roles"`: the roles **this image** serves (`log::Roles`), as an array in the fixed order
///   `mm`, `tool`: `["mm"]` (the Metamod plugin role: `CreateInterface("ISmmPlugin")` was served
///   or a `Load` completed here), `["tool"]` (the tools framework's `ToolSystem2_001`), both, or
///   `[]` (neither). One DLL image can serve both roles at once under the Wine tools, and every
///   field here describes that one image; each log line carries the same set as its tag.
///
/// # Consumers
///
/// Nothing in `client-mcp` or `hammer-mcp` reads this line (checked when v2 was written): the
/// consumers are other Metamod plugins asking for `McpBridge_001` through `OnMetamodQuery`
/// (`crate::plugin`), and this crate's tests. That is why the shape could change with
/// [`VERSION`] instead of a compatibility window.
#[must_use]
pub fn status_json() -> String {
    status_json_for(
        plugin::is_present(),
        s2sdk::hook::api().is_some(),
        toolsys::ticks(),
        crate::log::roles(),
        crate::gameframe::ticks(),
        crate::gameframe::tick_source(),
        crate::gameframe::state(),
        crate::gameframe::off_thread(),
    )
}

/// [`status_json`] over explicit values: one place builds the bytes, so the tests can pin them
/// with a role set they choose instead of whatever the test binary happens to serve.
#[must_use]
#[allow(clippy::too_many_arguments)] // One parameter per status member; that is the point.
pub fn status_json_for(
    metamod_present: bool,
    khook_installed: bool,
    ticks: u64,
    roles: crate::log::Roles,
    gameframe_ticks: u64,
    tick_source: &str,
    gameframe_state: &str,
    gameframe_off_thread: bool,
) -> String {
    format!(
        "{{\"metamod\":\"{}\",\"khook\":{},\"tick\":{},\"roles\":{},\
         \"tickSource\":\"{}\",\"gameframeTicks\":{},\"gameframe\":\"{}\",\
         \"gameframeOffThread\":{}}}",
        if metamod_present { "present" } else { "absent" },
        khook_installed,
        ticks,
        roles.json(),
        tick_source,
        gameframe_ticks,
        gameframe_state,
        gameframe_off_thread
    )
}

/// `McpBridge_vtable::GetStatus`: write [`status_json`] into `buf` and answer its length.
unsafe extern "C" fn get_status(this: *mut McpBridge, buf: *mut u8, len: usize) -> usize {
    crate::guard(0, || {
        let _ = this;
        let status = status_json();
        write_cstr(buf, len, &status);
        status.len()
    })
}

/// Write `text` into `buf` as a NUL-terminated string, truncating to fit `len` bytes.
///
/// `buf` may be null (a caller asking only for the length, as with `snprintf`) or `len` may be
/// zero; nothing is written then.
fn write_cstr(buf: *mut u8, len: usize, text: &str) {
    if buf.is_null() || len == 0 {
        return;
    }
    let bytes = text.as_bytes();
    let n = bytes.len().min(len - 1);
    // SAFETY: the caller guarantees `buf` is writable for `len` bytes (the vtable's contract),
    // and `n <= len - 1`, so the copy and the terminator both stay inside the buffer.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        *buf.add(n) = 0;
    }
}

/// The status line read back through a caller-shaped buffer, for the tests and the probe: the
/// `GetStatus` contract above, applied to a Rust `String`.
#[must_use]
pub fn status_via_vtable(capacity: usize) -> (usize, String) {
    let mut buf = vec![0u8; capacity];
    // SAFETY: `buf` is writable for `capacity` bytes, which is what the entry expects, and
    // `this` is the object the entry belongs to (`VTABLE` is that object's table).
    let len = unsafe { (VTABLE.GetStatus)(object().cast::<McpBridge>(), buf.as_mut_ptr(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    (
        len,
        String::from_utf8(buf[..end].to_vec()).expect("the status line is ASCII"),
    )
}

/// The status line's length is what `GetStatus` reports, and the object's layout is the C++
/// class's on this ABI.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_layout_is_vtable_version_reserved() {
        assert_eq!(core::mem::size_of::<McpBridge>(), 16);
        assert_eq!(core::mem::offset_of!(McpBridge, vt), 0);
        assert_eq!(core::mem::offset_of!(McpBridge, version), 8);
        assert_eq!(core::mem::offset_of!(McpBridge, reserved), 12);
        assert_eq!(core::mem::size_of::<McpBridge_vtable>(), 8);
    }

    #[test]
    fn object_points_at_the_vtable_and_reports_its_version() {
        // SAFETY: `object()` is this module's static object, `vt` points at `VTABLE`.
        let obj: *const McpBridge = object().cast();
        assert_eq!(unsafe { (*obj).vt }, vtable());
        assert_eq!(unsafe { (*obj).version }, VERSION);
    }

    /// The entry takes the object pointer first, as every C++ vtable call passes it, and it
    /// writes into the *buffer* argument - not into `this`. A slot declared without `this`
    /// would be handed the object as `buf` by a C++ caller and would corrupt it, so the object
    /// is checked unchanged after a call.
    #[test]
    fn get_status_writes_the_buffer_and_leaves_the_object_alone() {
        let obj: *const McpBridge = object().cast();
        // SAFETY: `object()` is this module's static object; reading its two words is fine.
        let before = unsafe { ((*obj).vt, (*obj).version, (*obj).reserved) };

        let mut buf = [0u8; 96];
        // SAFETY: `obj` is the object whose table `VTABLE` is, and `buf` is writable for its
        // own length - the call a C++ `obj->GetStatus(buf, n)` makes.
        let len = unsafe { (VTABLE.GetStatus)(object().cast::<McpBridge>(), buf.as_mut_ptr(), buf.len()) };
        assert!(len > 0);
        assert_eq!(buf[0], b'{', "the JSON lands in the buffer");

        // SAFETY: as above.
        let after = unsafe { ((*obj).vt, (*obj).version, (*obj).reserved) };
        assert_eq!(before, after, "the object is untouched");
    }

    #[test]
    fn status_is_one_json_line_and_a_small_buffer_truncates_without_overflowing() {
        // v3's line (roles + tick source + the GameFrame counter and state) is ~150 bytes.
        let (full_len, line) = status_via_vtable(256);
        assert_eq!(full_len, line.len(), "the length excludes the terminator");
        assert!(line.starts_with("{\"metamod\":\"absent\""), "{line}");
        assert!(line.contains("\"roles\":["), "{line}");
        assert!(line.ends_with('}'), "{line}");
        assert!(!line.contains('\n'), "{line}");

        // A 8-byte buffer holds 7 bytes and the terminator; the reported length is the whole
        // line's, so a caller can size its buffer and ask again.
        let mut buf = [0x7fu8; 8];
        // SAFETY: as in `status_via_vtable`.
        let reported = unsafe {
            (VTABLE.GetStatus)(object().cast::<McpBridge>(), buf.as_mut_ptr(), buf.len())
        };
        assert_eq!(reported, full_len);
        assert_eq!(buf[7], 0, "the terminator is inside the buffer");
        assert_eq!(&buf[..7], &line.as_bytes()[..7]);

        // A null buffer only asks for the length.
        // SAFETY: null is explicitly allowed by the entry's contract.
        let len_only = unsafe {
            (VTABLE.GetStatus)(object().cast::<McpBridge>(), ptr::null_mut(), 0)
        };
        assert_eq!(len_only, full_len);
    }

    /// v2's status line, byte for byte, for every role set - the wire change this version
    /// exists for (`"image"` -> `"roles"`), and v3's additions (the tick source, the GameFrame
    /// counter and the hook state). The v2 fields keep their names, order and meaning.
    #[test]
    fn status_bytes_for_every_role_set() {
        use crate::log::{Role, Roles};

        assert_eq!(VERSION, 3, "the status line's shape changed");
        let none = Roles::NONE;
        let mm = none.with(Role::Metamod);
        let tool = none.with(Role::Tool);
        let both = mm | tool;
        let absent = "not installed: no Metamod Load in this image";
        let installed = "installed";

        assert_eq!(
            status_json_for(false, false, 0, none, 0, "none", absent, false),
            format!(
                "{{\"metamod\":\"absent\",\"khook\":false,\"tick\":0,\"roles\":[],\
                 \"tickSource\":\"none\",\"gameframeTicks\":0,\"gameframe\":\"{absent}\",\
                 \"gameframeOffThread\":false}}"
            )
        );
        assert_eq!(
            status_json_for(true, true, 7, mm, 0, "gameframe", installed, true),
            "{\"metamod\":\"present\",\"khook\":true,\"tick\":7,\"roles\":[\"mm\"],\"tickSource\":\"gameframe\",\"gameframeTicks\":0,\"gameframe\":\"installed\",\"gameframeOffThread\":true}"
        );
        assert_eq!(
            status_json_for(false, true, 42, tool, 0, "tools", absent, false),
            "{\"metamod\":\"absent\",\"khook\":true,\"tick\":42,\"roles\":[\"tool\"],\"tickSource\":\"tools\",\"gameframeTicks\":0,\"gameframe\":\"not installed: no Metamod Load in this image\",\"gameframeOffThread\":false}"
        );
        assert_eq!(
            status_json_for(true, true, 3, both, 9, "tools", installed, false),
            "{\"metamod\":\"present\",\"khook\":true,\"tick\":3,\"roles\":[\"mm\",\"tool\"],\"tickSource\":\"tools\",\"gameframeTicks\":9,\"gameframe\":\"installed\",\"gameframeOffThread\":false}"
        );
        // The live line is that same builder over the live state.
        assert_eq!(
            status_json(),
            status_json_for(
                plugin::is_present(),
                s2sdk::hook::api().is_some(),
                toolsys::ticks(),
                crate::log::roles(),
                crate::gameframe::ticks(),
                crate::gameframe::tick_source(),
                crate::gameframe::state(),
                crate::gameframe::off_thread()
            )
        );
    }

}
