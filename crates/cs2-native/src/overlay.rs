//! Linux: run the game in a private user + mount namespace with kernel overlayfs mounted over
//! the install root (README "Linux: overlay"). The user's tree is the read-only lower layer; our
//! files are staged into the upper dir before the mount, and everything the game writes lands
//! there too. The mount exists only inside the namespace and goes away with its last process.

#![allow(unsafe_code)]

use crate::manifest::{op_bytes, under, write_atomic};
use crate::plan::Plan;
use anyhow::Context;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Stage `plan` into `upper` (emptied first: a previous session's files and game writes go).
/// Edits read the user's file from `root`.
pub fn stage(plan: &Plan, root: &Path, upper: &Path, work: &Path) -> anyhow::Result<Vec<String>> {
    for d in [upper, work] {
        if d.exists() {
            // overlayfs leaves work/work with mode 000 after use; make it removable
            let _ = make_writable(d);
            std::fs::remove_dir_all(d).with_context(|| format!("clear {}", d.display()))?;
        }
        std::fs::create_dir_all(d).with_context(|| format!("mkdir {}", d.display()))?;
    }
    let mut staged = Vec::new();
    for op in &plan.ops {
        let bytes = op_bytes(root, &op.source, &op.rel)?;
        let dst = under(upper, &op.rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        write_atomic(&dst, &bytes)?;
        staged.push(op.rel.clone());
    }
    Ok(staged)
}

/// Remove an overlay state dir (upper + work; work keeps a mode-000 dir after use).
pub fn clear(d: &Path) -> anyhow::Result<()> {
    if d.exists() {
        let _ = make_writable(d);
        std::fs::remove_dir_all(d).with_context(|| format!("remove {}", d.display()))?;
    }
    Ok(())
}

fn make_writable(d: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for e in std::fs::read_dir(d)? {
        let p = e?.path();
        if p.is_dir() && !p.is_symlink() {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
            let _ = make_writable(&p);
        }
    }
    Ok(())
}

fn cstr(p: &Path) -> anyhow::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains a NUL byte")
}

/// Make `cmd` enter a new user + mount namespace, mount the overlay over `root`, and `chdir`
/// into `cwd` (resolved after the mount, so it sees the merged tree) before it execs.
pub fn configure(cmd: &mut Command, root: &Path, upper: &Path, work: &Path, cwd: &Path) -> anyhow::Result<()> {
    let target = cstr(root)?;
    let chdir = cstr(cwd)?;
    // lowerdir=<root>: the mount point is resolved before the mount covers it
    let opts = format!(
        "lowerdir={},upperdir={},workdir={},userxattr",
        escape(root),
        escape(upper),
        escape(work)
    );
    let opts = CString::new(opts).context("overlay options contain a NUL byte")?;
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = format!("{uid} {uid} 1");
    let gid_map = format!("{gid} {gid} 1");
    let fs = c"overlay";
    let root_c = c"/";
    let setgroups = c"/proc/self/setgroups";
    let uid_path = c"/proc/self/uid_map";
    let gid_path = c"/proc/self/gid_map";
    // SAFETY: the closure runs in the forked child before exec and only makes async-signal-safe
    // calls (unshare, open, write, close, mount, chdir) on buffers prepared above.
    unsafe {
        cmd.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            write_proc(setgroups.as_ptr(), b"deny")?;
            write_proc(uid_path.as_ptr(), uid_map.as_bytes())?;
            write_proc(gid_path.as_ptr(), gid_map.as_bytes())?;
            if libc::mount(std::ptr::null(), root_c.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::mount(fs.as_ptr(), target.as_ptr(), fs.as_ptr(), 0, opts.as_ptr().cast()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(chdir.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

unsafe fn write_proc(path: *const libc::c_char, data: &[u8]) -> std::io::Result<()> {
    // SAFETY: the caller passes a NUL-terminated path; `data` is a valid buffer.
    unsafe {
        let fd = libc::open(path, libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, data.as_ptr().cast(), data.len());
        libc::close(fd);
        if n != data.len() as isize {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// overlayfs option values: `,` `:` and `\` must be escaped with a backslash.
fn escape(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, ',' | ':' | '\\') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Probe: can this process create the namespace and mount an overlay at all? Runs `true` the
/// same way the game will run, over a scratch tree under `scratch`.
pub fn probe(scratch: &Path) -> anyhow::Result<()> {
    let lower = scratch.join("lower");
    let upper = scratch.join("upper");
    let work = scratch.join("work");
    for d in [&lower, &upper, &work] {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(lower.join("lower.txt"), b"lower")?;
    std::fs::write(upper.join("upper.txt"), b"upper")?;
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "test -f lower.txt && test -f upper.txt && echo seen > game-wrote.txt"]);
    configure(&mut cmd, &lower, &upper, &work, &lower)?;
    let st = cmd.status().context("spawn the overlay probe")?;
    let wrote_upper = upper.join("game-wrote.txt").is_file();
    let wrote_lower = lower.join("game-wrote.txt").exists();
    let _ = make_writable(scratch);
    let _ = std::fs::remove_dir_all(scratch);
    if !st.success() || !wrote_upper || wrote_lower {
        anyhow::bail!(
            "the overlay probe failed (status {st}, upper write {wrote_upper}, lower write {wrote_lower})"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_escaping() {
        assert_eq!(escape(Path::new("/a,b:c\\d")), "/a\\,b\\:c\\\\d");
        assert_eq!(escape(Path::new("/s p/x")), "/s p/x");
    }

    /// Live on hosts that allow unprivileged user namespaces (skipped elsewhere): the game sees
    /// both layers, its writes land in upper, the lower tree is untouched.
    #[test]
    fn overlay_probe_or_skip() {
        let dir = crate::testutil::scratch("overlay-probe");
        match probe(&dir) {
            Ok(()) => {}
            Err(e) => eprintln!("overlay not available here, skipped: {e:#}"),
        }
    }
}
