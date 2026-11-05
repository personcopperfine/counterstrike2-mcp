//! Windows: `cs2mcp_launch.exe`, the launcher the Wine tools/client paths run instead of `cs2.exe`
//! (CONTRACT "Runtime").
//!
//! Why it exists: Metamod has to be in front of the engine's own `server.dll` at the moment the
//! engine asks for it, and the engine only asks after it has read `gameinfo.gi`. The launcher
//! gives the loader DLL a *suspended* process to install its IAT hook in, then resumes it, so
//! nothing about the game's files changes.
//!
//! The steps, in order:
//!
//! 0. the caller's tail must carry `-insecure` (VAC: injected code never runs in a secured
//!    session) - refused before anything is checked or started;
//! 1. `cs2.exe`, `cs2mcp_mmload.dll` (both next to this exe) and Metamod's loader
//!    (`..\..\csgo\addons\metamod\bin\win64\server.dll`, or an explicit `CS2MCP_MM_LOADER`) must
//!    exist - refused before anything is started;
//! 2. the Metamod path is put in `CS2MCP_MM_LOADER`, which the child inherits and the DLL reads;
//! 3. `CreateProcessW(cs2.exe, "<this exe's path>" + the caller's raw tail, CREATE_SUSPENDED)` -
//!    the tail is passed verbatim (see [`crate::cmdline`]), std handles are inherited, and the
//!    working directory is left alone so the caller's cwd keeps working;
//! 4. a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is attached (best effort, DESIGN §1);
//! 5. `CreateRemoteThread(LoadLibraryW, cs2mcp_mmload.dll)` is waited on for at most 30 s; a NULL
//!    module (the DLL's `DllMain` said `FALSE`) or a timeout terminates cs2.exe;
//! 6. resume, wait for as long as the game runs, and exit with cs2.exe's own exit code.
//!
//! Every failure before the resume terminates the suspended cs2.exe and exits 3 after one
//! `[cs2mcp-launch] FAIL: <reason>` line (CONTRACT); there is no path that starts the game without
//! the loader having said it is installed.

use core::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Environment::GetCommandLineW;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, GetExitCodeThread, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, INFINITE, LPTHREAD_START_ROUTINE,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use crate::{cmdline, log, paths};

/// The contract's launcher failure code: cs2.exe terminated, one FAIL line, exit 3.
const FAILURE_EXIT: i32 = 3;

/// Bound on the injected `LoadLibraryW` (DESIGN §1). The DLL's `DllMain` only patches an IAT slot,
/// so anything near this is a hung loader, not a slow boot.
const INJECT_TIMEOUT_MS: u32 = 30_000;

/// Run the launcher and return the process exit code.
pub fn run() -> i32 {
    let raw = raw_command_line();
    let (_, tail) = cmdline::split_program(&raw);
    // VAC (windows-native.md §4.1): injected code never runs in a session that could be
    // secured. Every launch path passes -insecure; one that does not is refused here, before
    // anything is created.
    if !cmdline::tail_has_insecure(tail) {
        return fail(
            "refusing to inject without -insecure on the command line (Metamod and the bridge must never run in a VAC-secured session)",
        );
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail(&format!("cannot read my own path: {error}")),
    };
    let Some(dir) = exe.parent() else {
        return fail(&format!("my own path has no directory: {}", exe.display()));
    };

    let cs2 = dir.join("cs2.exe");
    let loader_dll = dir.join("cs2mcp_mmload.dll");
    // DESIGN §1/§2: the launcher sits in `<game>\bin\win64`, Metamod in
    // `<game>\csgo\addons\metamod\bin\win64` (the standard Metamod layout). An explicit
    // `CS2MCP_MM_LOADER` wins - it is the same variable the DLL reads, so the two halves can never
    // disagree about which file is meant, and a test tree can point it somewhere else.
    let metamod = match std::env::var_os("CS2MCP_MM_LOADER").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => paths::windows_metamod_loader_for_exe_dir(dir),
    };

    for (what, path) in [
        ("cs2.exe", &cs2),
        ("the loader DLL", &loader_dll),
        ("Metamod's loader", &metamod),
    ] {
        if !path.is_file() {
            return fail(&format!("{what} is missing: {}", path.display()));
        }
    }
    if !metamod.is_absolute() {
        return fail(&format!(
            "Metamod's loader path is not absolute: {}",
            metamod.display()
        ));
    }

    // The hand-off between the two halves: `CreateProcessW` with a NULL environment block inherits
    // ours, so this one line is what the DLL reads on the other side.
    std::env::set_var("CS2MCP_MM_LOADER", &metamod);

    let command_line = cmdline::child_command_line(&cs2.to_string_lossy(), tail);
    log::line(log::LAUNCHER, &format!("metamod loader {}", metamod.display()));
    log::line(log::LAUNCHER, &format!("cmdline {command_line}"));

    let cs2_wide = wide(&cs2.to_string_lossy());
    let mut command_line_wide = wide(&command_line);
    let mut startup: STARTUPINFOW = unsafe { core::mem::zeroed() };
    startup.cb = core::mem::size_of::<STARTUPINFOW>() as u32;
    inherit_std_handles(&mut startup);
    let mut child: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `cs2_wide` and `command_line_wide` are NUL-terminated and outlive the call;
    // `startup`/`child` are fully initialised; the cwd is deliberately NULL (inherit ours, exactly
    // as cs2.exe would have had it).
    let created = unsafe {
        CreateProcessW(
            cs2_wide.as_ptr(),
            command_line_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles: the std handles selected above
            CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut child,
        )
    };
    if created == 0 {
        return fail(&format!(
            "CreateProcessW({}) failed: {}",
            cs2.display(),
            last_error()
        ));
    }

    // Best effort (DESIGN §1): with KILL_ON_JOB_CLOSE, cs2.exe cannot outlive the launcher even if
    // the launcher is killed. Wine may refuse either call; that is logged and the boot continues -
    // a stray game process after our own crash is better than refusing to start the game.
    let job = create_kill_on_close_job();
    if let Some(job) = job {
        // SAFETY: `job` is our job handle and `child.hProcess` is the suspended child.
        if unsafe { AssignProcessToJobObject(job, child.hProcess) } == 0 {
            log::line(
                log::LAUNCHER,
                &format!(
                    "job object: AssignProcessToJobObject refused ({}); cs2.exe may outlive the launcher",
                    last_error()
                ),
            );
        }
    }

    if let Err(reason) = inject(&child, &loader_dll) {
        // SAFETY: `child.hProcess` is the suspended process we just created.
        unsafe { TerminateProcess(child.hProcess, 1) };
        return fail(&reason);
    }

    // SAFETY: `child.hThread` is the suspended process's primary thread.
    let resumed = unsafe { ResumeThread(child.hThread) };
    if resumed == u32::MAX {
        // SAFETY: as above.
        unsafe { TerminateProcess(child.hProcess, 1) };
        return fail(&format!("ResumeThread failed: {}", last_error()));
    }
    // SAFETY: the thread handle is ours and no longer needed.
    unsafe { CloseHandle(child.hThread) };

    // SAFETY: `child.hProcess` is ours; INFINITE waits for the game to exit.
    let waited = unsafe { WaitForSingleObject(child.hProcess, INFINITE) };
    let mut code = 0u32;
    // SAFETY: `child.hProcess` is ours and the wait has returned.
    let read = unsafe { GetExitCodeProcess(child.hProcess, &mut code) };
    // SAFETY: our handles; the child has exited, so closing the job kills nothing.
    unsafe { CloseHandle(child.hProcess) };
    if let Some(job) = job {
        // SAFETY: our handle.
        unsafe { CloseHandle(job) };
    }

    if waited != WAIT_OBJECT_0 {
        return fail(&format!(
            "waiting for cs2.exe failed (wait status {waited:#x})"
        ));
    }
    if read == 0 {
        return fail(&format!("GetExitCodeProcess failed: {}", last_error()));
    }
    log::line(log::LAUNCHER, &format!("cs2.exe exited {code}"));
    // Exit code passthrough (CONTRACT): `podman wait` must see cs2's own code, e.g. `+quit` -> 0.
    code as i32
}

/// `[cs2mcp-launch] FAIL: <reason>` and the contract's exit code.
fn fail(reason: &str) -> i32 {
    log::fail(log::LAUNCHER, reason);
    FAILURE_EXIT
}

/// Inject `dll` into the suspended `child` with `CreateRemoteThread(LoadLibraryW)` and make sure it
/// actually loaded.
///
/// `LoadLibraryW` returns the module handle as the thread's exit code, so a NULL module (the DLL's
/// `DllMain` returned `FALSE`) is visible here as exit code 0 and is what stops the boot.
fn inject(child: &PROCESS_INFORMATION, dll: &Path) -> Result<(), String> {
    let path = wide(&dll.to_string_lossy());
    let bytes = path.len() * core::mem::size_of::<u16>();
    // SAFETY: `child.hProcess` is a live suspended process; the allocation is ours.
    let remote = unsafe {
        VirtualAllocEx(
            child.hProcess,
            std::ptr::null(),
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote.is_null() {
        return Err(format!("VirtualAllocEx({bytes} bytes) failed: {}", last_error()));
    }
    // SAFETY: `remote` is a fresh, writable `bytes`-sized page in the child; the source is the
    // UTF-16 path buffer of exactly that length.
    if unsafe {
        WriteProcessMemory(
            child.hProcess,
            remote as *const c_void,
            path.as_ptr().cast(),
            bytes,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = last_error();
        // SAFETY: our own allocation in the child.
        unsafe { VirtualFreeEx(child.hProcess, remote, 0, MEM_RELEASE) };
        return Err(format!("WriteProcessMemory of the DLL path failed: {error}"));
    }

    let kernel32 = module_handle("kernel32.dll")?;
    // SAFETY: kernel32 is loaded; the name is a NUL-terminated literal, and a failed lookup is a
    // normal `None`.
    let load_library = unsafe { GetProcAddress(kernel32, c"LoadLibraryW".as_ptr().cast()) }
        .ok_or_else(|| "kernel32.dll does not export LoadLibraryW".to_string())?;
    // SAFETY: the remote thread calls `LoadLibraryW(remote_path)`; a FARPROC and
    // LPTHREAD_START_ROUTINE are the same pointer with the same calling convention, and the
    // HMODULE return becomes the thread's exit code.
    let start: LPTHREAD_START_ROUTINE = Some(unsafe {
        core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(load_library)
    });
    // SAFETY: every argument belongs to the suspended child.
    let thread = unsafe {
        CreateRemoteThread(
            child.hProcess,
            std::ptr::null(),
            0,
            start,
            remote as *const c_void,
            0,
            std::ptr::null_mut(),
        )
    };
    if thread.is_null() {
        // SAFETY: our own allocation in the child.
        unsafe { VirtualFreeEx(child.hProcess, remote, 0, MEM_RELEASE) };
        return Err(format!(
            "CreateRemoteThread(LoadLibraryW) failed: {}",
            last_error()
        ));
    }

    // SAFETY: `thread` is a handle we own.
    let waited = unsafe { WaitForSingleObject(thread, INJECT_TIMEOUT_MS) };
    if waited != WAIT_OBJECT_0 {
        // The thread may still be running: the caller terminates the process, so the remote page is
        // left to the process teardown rather than freed under a live thread.
        // SAFETY: `thread` is ours.
        unsafe { CloseHandle(thread) };
        return Err(format!(
            "the injected LoadLibraryW did not return within {} s",
            INJECT_TIMEOUT_MS / 1000
        ));
    }
    let mut module = 0u32;
    // SAFETY: `thread` is ours and has finished.
    let read = unsafe { GetExitCodeThread(thread, &mut module) };
    // SAFETY: as above.
    unsafe { CloseHandle(thread) };
    // SAFETY: our own allocation in the child; the thread has returned.
    unsafe { VirtualFreeEx(child.hProcess, remote, 0, MEM_RELEASE) };

    if read == 0 {
        return Err(format!("GetExitCodeThread failed: {}", last_error()));
    }
    if module == 0 {
        return Err(
            "cs2mcp_mmload.dll refused to load (its [cs2mcp-mmload] FAIL line states why)"
                .to_string(),
        );
    }
    log::line(
        log::LAUNCHER,
        &format!("injected {} -> module {module:#x}", dll.display()),
    );
    Ok(())
}

/// A job object that kills its members when its last handle closes, or `None` if the OS refuses
/// (logged, not fatal - see the call site).
fn create_kill_on_close_job() -> Option<HANDLE> {
    // SAFETY: no attributes, no name; the returned handle is ours.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        log::line(
            log::LAUNCHER,
            &format!(
                "job object: CreateJobObjectW refused ({}); cs2.exe may outlive the launcher",
                last_error()
            ),
        );
        return None;
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    // BREAKAWAY_OK: a child that explicitly asks for CREATE_BREAKAWAY_FROM_JOB (an engine tool
    // spawning a compiler, say) still gets it instead of an access-denied CreateProcess; ordinary
    // children stay in the job and die with it.
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let size = core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    // SAFETY: `limits` is a fully initialised structure of exactly `size` bytes and the information
    // class matches its type.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size,
        )
    };
    if set == 0 {
        log::line(
            log::LAUNCHER,
            &format!(
                "job object: SetInformationJobObject(KILL_ON_JOB_CLOSE) refused ({}); cs2.exe may outlive the launcher",
                last_error()
            ),
        );
        // SAFETY: our handle, not used any more.
        unsafe { CloseHandle(job) };
        return None;
    }
    Some(job)
}

/// Give the child this process's own std handles when they are inheritable.
///
/// Under Wine they are the container's log pipe, so the game's console output must keep reaching
/// `podman logs`. The handles are only passed explicitly when all three are real and carry
/// `HANDLE_FLAG_INHERIT`; otherwise the flag is left off and the console defaults apply, which is
/// what a plain `bInheritHandles` launch does (and what the spike proved works).
fn inherit_std_handles(startup: &mut STARTUPINFOW) {
    // SAFETY: GetStdHandle/GetHandleInformation only read process state.
    let handles = unsafe {
        [
            GetStdHandle(STD_INPUT_HANDLE),
            GetStdHandle(STD_OUTPUT_HANDLE),
            GetStdHandle(STD_ERROR_HANDLE),
        ]
    };
    if handles
        .iter()
        .any(|handle| handle.is_null() || *handle == INVALID_HANDLE_VALUE)
    {
        return;
    }
    for handle in handles {
        let mut flags = 0u32;
        // SAFETY: a handle we just obtained from GetStdHandle.
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0
            || flags & HANDLE_FLAG_INHERIT == 0
        {
            return;
        }
    }
    startup.dwFlags |= STARTF_USESTDHANDLES;
    startup.hStdInput = handles[0];
    startup.hStdOutput = handles[1];
    startup.hStdError = handles[2];
}

/// `GetModuleHandleW` for a NUL-terminated ASCII name.
fn module_handle(name: &str) -> Result<HANDLE, String> {
    let wide_name = wide(name);
    // SAFETY: the name is NUL-terminated; the call neither loads nor blocks.
    let handle = unsafe { GetModuleHandleW(wide_name.as_ptr()) };
    if handle.is_null() {
        return Err(format!("{name} is not loaded"));
    }
    Ok(handle)
}

/// The process's own command line, exactly as the platform gave it.
fn raw_command_line() -> String {
    // SAFETY: GetCommandLineW returns this process's immutable command line.
    let pointer = unsafe { GetCommandLineW() };
    // SAFETY: the returned pointer is NUL-terminated for the life of the process.
    unsafe { wide_to_string(pointer) }
}

/// `str` -> NUL-terminated UTF-16, the form every `W` API here takes.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A NUL-terminated UTF-16 string as a `String` (lossy: nothing here needs undecodable units).
///
/// # Safety
/// `pointer` must be NUL-terminated and readable up to the terminator.
unsafe fn wide_to_string(pointer: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: the caller guarantees a terminator.
    while unsafe { *pointer.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units are initialised and precede the terminator.
    String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(pointer, len) })
}

/// `std::io::Error` for the last failed Win32 call.
fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}
