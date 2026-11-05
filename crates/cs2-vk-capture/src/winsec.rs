//! Windows: who owns the capture directory. The Unix side checks `st_uid == geteuid()`; here the
//! directory's owner SID must be this process's user SID, or its token's default owner (the owner
//! the system gives the files this token creates).
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenOwner, TokenUser, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `GetTokenInformation` into a `u64`-aligned buffer (the SID structs hold pointers).
unsafe fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<Vec<u64>> {
    let mut len = 0u32;
    GetTokenInformation(token, class, null_mut(), 0, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    (GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) != 0).then_some(buf)
}

/// `true` when `path`'s owner is this process's user or its token's default owner.
pub fn owned_by_me(path: &Path) -> bool {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: plain Win32 calls on locals; `sd` is freed with LocalFree, the token handle closed,
    // and the SIDs compared only while the buffers that hold them are alive.
    unsafe {
        let mut owner: PSID = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != ERROR_SUCCESS || owner.is_null() {
            return false;
        }
        let mut token: HANDLE = null_mut();
        let mut ok = false;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0 {
            if let Some(user) = token_info(token, TokenUser) {
                let sid = (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid;
                ok = EqualSid(owner, sid) != 0;
            }
            if !ok {
                if let Some(def) = token_info(token, TokenOwner) {
                    let sid = (*def.as_ptr().cast::<TOKEN_OWNER>()).Owner;
                    ok = EqualSid(owner, sid) != 0;
                }
            }
            CloseHandle(token);
        }
        LocalFree(sd);
        ok
    }
}
