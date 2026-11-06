//! Windows: the registry, process control and the supervisor's job object. Thin wrappers over windows-sys; the logic lives in the portable modules.

#![allow(unsafe_code)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

/// `CreateProcess` flags used with `std::os::windows::process::CommandExt::creation_flags`.
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const STILL_ACTIVE: u32 = 259;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: a handle this module opened and owns.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut bytes = (buf.len() * 2) as u32;
    // SAFETY: valid NUL-terminated names, a buffer of `bytes` bytes.
    let rc = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
    };
    if rc != 0 {
        return None;
    }
    let n = (bytes as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}

/// Steam's install dir: `HKCU\Software\Valve\Steam\SteamPath`, then the machine-wide
/// `InstallPath` (32-bit view).
/// Make this process's own std handles non-inheritable. Rust's `Command` creates every child with
/// `bInheritHandles`, so a detached supervisor would otherwise inherit (and hold open) whatever
/// pipe `up`'s caller reads, and the caller would wait for the game to exit. The child's
/// explicitly redirected handles are still passed.
pub fn no_inherit_std_handles() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: plain queries/flag changes on this process's std handles; failure is harmless.
        unsafe {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// This process's Terminal Services session id (0: the services' session).
pub fn session_id() -> Option<u32> {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    let mut id = 0u32;
    // SAFETY: a query on this process into a local u32.
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut id) };
    (ok != 0).then_some(id)
}

/// This process's window station is the visible, interactive one (`WinSta0` of a logged-on
/// desktop), not a service's or a non-interactive logon's.
pub fn window_station_visible() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS};
    const WSF_VISIBLE: u32 = 1;
    // SAFETY: the station handle is not owned (not closed); the query writes into a local struct.
    unsafe {
        let ws = GetProcessWindowStation();
        if ws.is_null() {
            return false;
        }
        let mut flags: USEROBJECTFLAGS = std::mem::zeroed();
        let mut need = 0u32;
        let ok = GetUserObjectInformationW(ws, UOI_FLAGS, (&mut flags as *mut USEROBJECTFLAGS).cast(), std::mem::size_of::<USEROBJECTFLAGS>() as u32, &mut need);
        ok != 0 && flags.dwFlags & WSF_VISIBLE != 0
    }
}

/// An error message box (blocks until it is closed: run it in a detached `show-dialog` child).
pub fn message_box(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST};
    let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (t, m) = (w(title), w(text));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), m.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST);
    }
}

pub fn steam_path_from_registry() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath") {
        v.push(PathBuf::from(p.replace('/', "\\")));
    }
    if let Some(p) = reg_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath") {
        v.push(PathBuf::from(p));
    }
    v
}

fn image_path(pid: u32) -> Option<PathBuf> {
    // SAFETY: plain query; the handle is closed by `Handle`.
    let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if h.0.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32768];
    let mut n = buf.len() as u32;
    // SAFETY: a buffer of `n` u16s.
    if unsafe { QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut n) } == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buf[..n as usize])))
}

/// Pids of every process named `name` (case-insensitive), wherever it lives.
pub fn processes_named(name: &str) -> Vec<u32> {
    snapshot().into_iter().filter(|(_, _, n)| n.eq_ignore_ascii_case(name)).map(|(p, _, _)| p).collect()
}

/// `(pid, parent pid, exe name)` of every process.
fn snapshot() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    // SAFETY: snapshot handle closed by `Handle`; the entry struct is sized for the call.
    unsafe {
        let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0));
        if snap.0 == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = core::mem::zeroed();
        e.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap.0, &mut e);
        while ok != 0 {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
            ok = Process32NextW(snap.0, &mut e);
        }
    }
    out
}

/// Processes named `names` (case-insensitive) whose image lives under `root`, plus those whose
/// image can no longer be read (a process deep in teardown, FIXME 9) but whose parent is one of
/// `parents` (our launcher pids: such a `cs2.exe` is ours wherever its image went). The path is
/// `None` for the second kind.
pub fn processes_under(root: &Path, names: &[&str], parents: &[u32]) -> Vec<(u32, Option<PathBuf>)> {
    let root_l = root.to_string_lossy().to_lowercase();
    let root_l = root_l.trim_end_matches('\\').to_string() + "\\";
    snapshot()
        .into_iter()
        .filter(|(_, _, n)| names.iter().any(|x| x.eq_ignore_ascii_case(n)))
        .filter_map(|(pid, ppid, _)| match image_path(pid) {
            Some(p) if p.to_string_lossy().to_lowercase().starts_with(&root_l) => Some((pid, Some(p))),
            Some(_) => None,
            None if parents.contains(&ppid) => Some((pid, None)),
            None => None,
        })
        .collect()
}

/// Is this process in a job object already (any job: the caller asks before joining its own)?
pub fn in_job() -> bool {
    let mut inside = 0;
    // SAFETY: the pseudo handle of this process; a null job asks about any job.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), core::ptr::null_mut(), &mut inside) };
    ok != 0 && inside != 0
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: plain query.
    let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if h.0.is_null() {
        return false;
    }
    let mut code = 0u32;
    // SAFETY: our handle.
    unsafe { GetExitCodeProcess(h.0, &mut code) != 0 && code == STILL_ACTIVE }
}

/// The creation time of `pid` (100 ns units since 1601, `GetProcessTimes`): with the pid it names
/// one process for good, so a recorded pid that Windows reused is told apart (`procid`).
pub fn process_start(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    // SAFETY: plain query.
    let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if h.0.is_null() {
        return None;
    }
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (z, z, z, z);
    // SAFETY: our handle and four out pointers.
    if unsafe { GetProcessTimes(h.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// Terminate `pid` (exit code 1). `Ok(false)` when it was already gone.
pub fn terminate(pid: u32) -> std::io::Result<bool> {
    // SAFETY: plain open/terminate on a pid we recorded.
    let h = Handle(unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if h.0.is_null() {
        return Ok(false);
    }
    if unsafe { TerminateProcess(h.0, 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// Wait for `pid` to exit; `true` when it did (or was not there) within `ms`.
///
/// Only a pid that does not exist (`ERROR_INVALID_PARAMETER`) counts as gone. A process this
/// token may not wait on (`ERROR_ACCESS_DENIED`: e.g. a supervisor started elevated) is polled
/// through a query handle instead; if even that is refused, the answer is "not gone" (`false`),
/// never a guess that lets a restore run under a live game (FIXME 8).
pub fn wait_pid(pid: u32, ms: u32) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    // SAFETY: plain open/wait.
    let h = Handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) });
    if !h.0.is_null() {
        return unsafe { WaitForSingleObject(h.0, ms) == WAIT_OBJECT_0 };
    }
    if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_ACCESS_DENIED) {
        return true;
    }
    // SAFETY: plain open; the limited query right is granted to far more callers than SYNCHRONIZE.
    let q = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) });
    if q.0.is_null() {
        return false;
    }
    let until = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(ms));
    loop {
        let mut code = 0u32;
        // SAFETY: a valid query handle and an out pointer.
        if unsafe { GetExitCodeProcess(q.0, &mut code) } == 0 {
            return false;
        }
        if code != STILL_ACTIVE {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A job object with `KILL_ON_JOB_CLOSE` that the **calling process** joins, so everything it
/// starts afterwards is in it (no race between `CreateProcess` and the assignment) and the game
/// dies with the supervisor if the supervisor dies.
pub struct Job(HANDLE);

// SAFETY: a kernel handle; used from one thread at a time.
unsafe impl Send for Job {}

impl Job {
    pub fn create_and_join(name: Option<&str>) -> std::io::Result<Job> {
        let n = name.map(wide);
        // SAFETY: a NUL-terminated name or NULL (unnamed); default security.
        let h = unsafe { CreateJobObjectW(std::ptr::null(), n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr())) };
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(h);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the struct matches the information class.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the pseudo handle of this process.
        if unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: our handle. Closing the last handle kills whatever is left in the job, the
        // calling process included: only drop it on the way out.
        unsafe { CloseHandle(self.0) };
    }
}
