//! Host tests for the one exported symbol: linked against the rlib half of the crate, so these
//! call the real `CreateInterface` (no mock, no stub).

use core::ffi::{c_int, c_void};
use core::ptr;

use cs2_bridge::CreateInterface;

/// The interface version string the tools framework asks for (`s2sdk.h:55`).
const TOOLSYSTEM2_VERSION: &core::ffi::CStr = c"ToolSystem2_001";

/// `"ToolSystem2_001"` is served on Windows (the tools' tool plugin) and refused on Linux, where
/// this host runs: null and `*ret = 1`. The C bridge's `ret` convention is `0` = returned an
/// object, `1` = not found (`mcp_bridge.c:6848`).
#[test]
fn tool_system_2_object_is_windows_only() {
    let mut ret: c_int = -1;
    // SAFETY: `ret` outlives the call and the name is a valid NUL-terminated literal.
    let object = unsafe { CreateInterface(TOOLSYSTEM2_VERSION.as_ptr(), &mut ret) };

    if cfg!(windows) {
        assert!(!object.is_null(), "Windows must serve the tool plugin");
        assert_eq!(ret, 0, "*ret = 0 means 'object returned'");
    } else {
        assert!(object.is_null(), "Linux has no tool plugin yet");
        assert_eq!(ret, 1, "*ret = 1 means 'not found'");
    }
}

/// A name the module does not implement - and a null `ret` - must not crash: the C bridge checks
/// `if (ret)` before writing (`mcp_bridge.c:6848`).
#[test]
fn unknown_name_with_null_ret_is_refused_not_written() {
    // SAFETY: the name is a valid NUL-terminated literal; a null `ret` is explicitly allowed.
    let object = unsafe { CreateInterface(c"x".as_ptr(), ptr::null_mut()) };
    assert!(object.is_null());
}

/// A null `name` is refused rather than dereferenced (the C `seq` would read it); `ret` still
/// gets the "not found" answer.
#[test]
fn null_name_is_refused() {
    let mut ret: c_int = -1;
    // SAFETY: a null `name` is the case under test; `ret` outlives the call.
    let object: *mut c_void = unsafe { CreateInterface(ptr::null(), &mut ret) };
    assert!(object.is_null());
    assert_eq!(ret, 1);
}
