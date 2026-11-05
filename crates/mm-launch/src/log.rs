//! One line per event, written straight to stderr: `[cs2mcp-launch] ...` / `[cs2mcp-mmload] ...`
//! (the tags are fixed by the cross-lane contract).
//!
//! Not `eprintln!`: the DLL calls this from `DllMain` under the loader lock (no stdio lock, no
//! allocation of a `Stderr` handle, no panic on a failed write) and the launcher must survive a
//! closed stderr - `eprintln!` panics when the write fails, which would turn "no log" into "the
//! game did not start".

/// The launcher's tag.
pub const LAUNCHER: &str = "[cs2mcp-launch]";

/// The loader's tag, Windows DLL and Linux preload alike.
pub const LOADER: &str = "[cs2mcp-mmload]";

/// Write `<tag> <message>` and a newline to stderr. Failures are ignored: a log line is never
/// allowed to decide the outcome.
pub fn line(tag: &str, message: &str) {
    let mut text = String::with_capacity(tag.len() + message.len() + 2);
    text.push_str(tag);
    text.push(' ');
    text.push_str(message);
    text.push('\n');
    write_stderr(text.as_bytes());
}

/// The `[tag] FAIL: <reason>` form both halves use on their fail-closed paths.
pub fn fail(tag: &str, reason: &str) {
    let mut text = String::with_capacity(tag.len() + reason.len() + 8);
    text.push_str(tag);
    text.push_str(" FAIL: ");
    text.push_str(reason);
    text.push('\n');
    write_stderr(text.as_bytes());
}

#[cfg(unix)]
fn write_stderr(bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: the buffer and length are valid for the call; `write` on fd 2 needs no setup,
        // allocates nothing and cannot unwind into a `dlopen` caller.
        let n = unsafe {
            libc::write(
                2,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
}

#[cfg(windows)]
fn write_stderr(bytes: &[u8]) {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};

    // SAFETY: both calls are plain kernel32 entry points; the handle is only used if it is a real
    // one, and a failed write is ignored on purpose.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() {
            return;
        }
        let mut written = 0u32;
        WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn write_stderr(bytes: &[u8]) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tags_are_the_contract_lines() {
        assert_eq!(LAUNCHER, "[cs2mcp-launch]");
        assert_eq!(LOADER, "[cs2mcp-mmload]");
    }
}
