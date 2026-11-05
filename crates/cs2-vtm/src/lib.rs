//! VToolMessage: post one KV3 record into a running `-tools` session's channel. A port of
//! `vtmsend.exe` (`idabins/worknotes/vtmsend/vtmsend.c`, schema in its header), shared by
//! `cs2mcp-native` (the readiness ladder's `tool_request cs2_bridge`) and `hammer-mcp` on a
//! Windows host (its `console_live` fallback, `run_map` and the first-map `open_asset`), where
//! there is no container to run `vtmsend.exe` in.
//!
//! The objects are session-local (no `Global\` prefix), so the poster must run in the game's
//! logon session: a process started from the desktop works, one started over ssh gets
//! [`Error::ChannelDown`] even while the game runs.
//!
//! ```text
//! ValveSMMut:VToolMessage  named mutex guarding appends
//! ValveSMMem:VToolMessage  named file mapping, 0x100004 bytes
//! map+0x00 u32 total size   map+0x04 u32 creator version (1)   map+0x08 u32 highest seen
//! map+0x0C u64 payload size (0x100000)   map+0x14 u64 write cursor (payload-relative, init 32)
//! map+0x1C u64 sequence     records at map+0x04+cursor: { u64 seq; u64 len; u8 bytes[len] }
//! ```

#![cfg_attr(not(windows), forbid(unsafe_code))]

use std::fmt;

/// Why a post did not happen. Every variant means **nothing was written** into the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The mutex or the mapping does not exist in this logon session: no `-tools` session runs,
    /// toolframework2 has not created the channel yet, or the poster is in another logon
    /// session (ssh). The text starts with `OpenMutex failed`, vtmsend's own wording.
    ChannelDown(String),
    /// The channel exists but refused the record (creator version, a full payload area, the
    /// mutex not released in time, a view that would not map).
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::ChannelDown(s) | Error::Refused(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

/// A KV3 text payload: the header and the body in braces (runtime.sh `put_payload`;
/// `cs2_mcp_common::runtime::docker::vtm_payload` writes the same bytes for the container).
pub fn payload(body: &str) -> String {
    format!(
        "<!-- kv3 encoding:text:version{{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d}} format:generic:version{{7412167c-06e9-4698-aff2-e63eb59037e7}} -->\n{{\n{body}\n}}\n"
    )
}

/// `"` and `\` escaped for a KV3 string (the two characters the runtime.sh recipe escapes).
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The bridge load (runtime.sh `load_rs_bridge`).
pub fn bridge_request() -> String {
    payload("\t_message = \"tool_request\"\n\ttool_name = \"cs2_bridge\"\n\t_data = -1")
}

/// A console command (`command` message).
pub fn command(cmd: &str) -> String {
    payload(&format!("\t_message = \"command\"\n\t_data = \"{}\"", escape(cmd)))
}

/// An `open_asset` (runtime.sh `open`): a content-relative asset path; a `.vmdl` loads ModelDoc.
pub fn open_asset(asset: &str) -> String {
    payload(&format!("\t_message = \"open_asset\"\n\t_data = \"{}\"", escape(asset)))
}

/// Append one record into the channel's payload area (pure: the mapped bytes from offset 0).
/// Returns the sequence number written. The text is stored with its NUL terminator, as
/// vtmsend does.
pub fn append(map: &mut [u8], text: &str) -> Result<u64, Error> {
    let rd32 = |m: &[u8], o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
    let rd64 = |m: &[u8], o: usize| u64::from_le_bytes(m[o..o + 8].try_into().unwrap());
    if map.len() < 0x24 {
        return Err(Error::Refused("mapping too small".into()));
    }
    if rd32(map, 0x04) != 1 {
        return Err(Error::Refused(format!("creator version {} != 1", rd32(map, 0x04))));
    }
    if rd32(map, 0x08) < 1 {
        map[0x08..0x0C].copy_from_slice(&1u32.to_le_bytes());
    }
    let payload_size = rd64(map, 0x0C);
    let cursor = rd64(map, 0x14).max(32);
    let seq = rd64(map, 0x1C);
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let len = bytes.len() as u64;
    if cursor + 16 + len > payload_size || (0x04 + cursor + 16 + len) as usize > map.len() {
        return Err(Error::Refused("channel payload area is full".into()));
    }
    let at = 0x04 + cursor as usize;
    map[at..at + 8].copy_from_slice(&seq.to_le_bytes());
    map[at + 8..at + 16].copy_from_slice(&len.to_le_bytes());
    map[at + 16..at + 16 + bytes.len()].copy_from_slice(&bytes);
    map[0x14..0x1C].copy_from_slice(&(cursor + 16 + len).to_le_bytes());
    map[0x1C..0x24].copy_from_slice(&(seq + 1).to_le_bytes());
    Ok(seq)
}

#[cfg(windows)]
mod win {
    use super::Error;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Memory::{
        MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, FILE_MAP_WRITE,
    };
    use windows_sys::Win32::System::Threading::{OpenMutexW, ReleaseMutex, WaitForSingleObject, MUTEX_ALL_ACCESS};

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: a handle this module opened and owns.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// The channel's objects (open = the channel is up).
    pub(super) struct Vtm {
        mutex: Handle,
        map: Handle,
    }

    pub(super) fn open() -> Result<Vtm, Error> {
        let mn = wide("ValveSMMut:VToolMessage");
        // SAFETY: NUL-terminated name.
        let mutex = Handle(unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, mn.as_ptr()) });
        if mutex.0.is_null() {
            return Err(Error::ChannelDown(
                "OpenMutex failed - is a -tools session running in this logon session?".into(),
            ));
        }
        let fm = wide("ValveSMMem:VToolMessage");
        // SAFETY: NUL-terminated name.
        let map = Handle(unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, fm.as_ptr()) });
        if map.0.is_null() {
            return Err(Error::ChannelDown("OpenFileMapping failed".into()));
        }
        Ok(Vtm { mutex, map })
    }

    /// vtmsend: lock, append one record, unlock.
    pub(super) fn post(text: &str) -> Result<u64, Error> {
        let v = open()?;
        // SAFETY: the mapping handle is open; the view is unmapped below.
        let view = unsafe { MapViewOfFile(v.map.0, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            return Err(Error::Refused("MapViewOfFile failed".into()));
        }
        // SAFETY: the mapping header says how large it is (0x100004); the view is at least that.
        let size = unsafe { *(view.Value as *const u32) } as usize;
        let size = size.clamp(0x24, 0x0010_0004);
        // SAFETY: an open mutex handle.
        let w = unsafe { WaitForSingleObject(v.mutex.0, 5000) };
        let r = if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            // SAFETY: the mutex is held; `size` bytes of the view are mapped.
            let slice = unsafe { std::slice::from_raw_parts_mut(view.Value as *mut u8, size) };
            let r = super::append(slice, text);
            // SAFETY: held by this thread (the wait above).
            unsafe { ReleaseMutex(v.mutex.0) };
            r
        } else {
            Err(Error::Refused("timed out waiting for the VToolMessage mutex".into()))
        };
        // SAFETY: the view mapped above.
        unsafe { UnmapViewOfFile(view) };
        r
    }
}

/// Is the channel up in this logon session (toolframework2 Init created the mutex)?
#[cfg(windows)]
pub fn channel_up() -> bool {
    win::open().is_ok()
}

/// Post `text` (a [`payload`]); the record's sequence number.
#[cfg(windows)]
pub fn post(text: &str) -> Result<u64, Error> {
    win::post(text)
}

/// Not Windows: the channel is the Windows tools' shared memory; there is none.
#[cfg(not(windows))]
pub fn channel_up() -> bool {
    false
}

/// Not Windows: always [`Error::ChannelDown`].
#[cfg(not(windows))]
pub fn post(_text: &str) -> Result<u64, Error> {
    Err(Error::ChannelDown("OpenMutex failed - VToolMessage is a Windows -tools channel".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_vtmsend_records() {
        let mut m = vec![0u8; 0x400];
        m[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());
        m[0x0C..0x14].copy_from_slice(&0x300u64.to_le_bytes());
        assert_eq!(append(&mut m, "ab"), Ok(0));
        // record at 0x04 + 32: seq 0, len 3, "ab\0"
        assert_eq!(&m[0x24..0x2C], &0u64.to_le_bytes());
        assert_eq!(&m[0x2C..0x34], &3u64.to_le_bytes());
        assert_eq!(&m[0x34..0x37], b"ab\0");
        assert_eq!(u64::from_le_bytes(m[0x14..0x1C].try_into().unwrap()), 32 + 16 + 3);
        assert_eq!(u32::from_le_bytes(m[0x08..0x0C].try_into().unwrap()), 1);
        assert_eq!(append(&mut m, "c"), Ok(1));
        let mut bad = vec![0u8; 0x40];
        assert!(matches!(append(&mut bad, "x"), Err(Error::Refused(_))));
    }

    #[test]
    fn a_full_payload_area_refuses_and_writes_nothing() {
        let mut m = vec![0u8; 0x100];
        m[0x04..0x08].copy_from_slice(&1u32.to_le_bytes());
        m[0x0C..0x14].copy_from_slice(&0x40u64.to_le_bytes());
        let before = m.clone();
        let long = "x".repeat(64);
        assert_eq!(append(&mut m, &long), Err(Error::Refused("channel payload area is full".into())));
        // only the "highest seen" word may change (vtmsend sets it before the size check)
        assert_eq!(&m[0x0C..], &before[0x0C..]);
    }

    #[test]
    fn payload_texts() {
        assert!(bridge_request().contains("tool_name = \"cs2_bridge\""));
        assert!(command("echo \"a\\b\"").contains("_data = \"echo \\\"a\\\\b\\\"\""));
        assert!(open_asset("models/a.vmdl").contains("_message = \"open_asset\"\n\t_data = \"models/a.vmdl\""));
        assert!(payload("x").starts_with("<!-- kv3 encoding:text"));
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_the_channel_is_down() {
        assert!(!channel_up());
        assert!(matches!(post("x"), Err(Error::ChannelDown(m)) if m.starts_with("OpenMutex failed")));
    }
}
