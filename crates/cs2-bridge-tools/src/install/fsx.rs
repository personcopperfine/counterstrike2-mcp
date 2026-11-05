//! File-system helpers shared by both platforms and by the in-namespace tree ops: messages and
//! exits, our temp files (registered, removed on every exit path), the atomic same-dir writes,
//! `cp -p`, sha256, `realpath -m` and `dirname` with the shell's semantics (the backup records
//! store paths spelled the way the shell tool spelled them, so the spelling rules must match).

use std::fmt::Display;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// A finished command: the message (if any) is already printed, the value is the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop(pub u8);

pub type R<T> = Result<T, Stop>;

/// One progress/diagnostic line on stderr (stdout carries only the tree ops' JSON results).
pub fn say(msg: impl Display) {
    eprintln!("[install] {msg}");
}

/// Exit 1 with a FAIL line.
pub fn die(msg: impl Display) -> Stop {
    say(format_args!("FAIL: {msg}"));
    Stop(1)
}

/// Exit 4 (refused: busy, or the lock is held), the code the launch paths map to "busy".
pub fn die4(msg: impl Display) -> Stop {
    say(format_args!("REFUSED(busy): {msg}"));
    Stop(4)
}

/// `result.or_die(|| "what failed")`: an I/O (or any) error becomes a FAIL line naming it.
pub trait OrDie<T> {
    fn or_die(self, what: impl FnOnce() -> String) -> R<T>;
}

impl<T, E: Display> OrDie<T> for Result<T, E> {
    fn or_die(self, what: impl FnOnce() -> String) -> R<T> {
        self.map_err(|e| die(format_args!("{}: {e}", what())))
    }
}

// ----- our temps ----------------------------------------------------------------------------------

/// Every temp file we create, and the backup `.partial` dir being built. Removed by [`cleanup`],
/// which runs on every return path of the command and from the signal thread (see
/// [`install_signal_cleanup`]): a killed or faulted run leaves none of them behind. A SIGKILLed
/// run's temps survive; the next run sweeps them by their exact name ([`sweep_dest_temps`]).
static TEMPS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static PARTIAL: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn register_temp(path: &Path) {
    TEMPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(path.to_path_buf());
}

pub fn set_partial(dir: Option<PathBuf>) {
    *PARTIAL.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

pub fn cleanup() {
    for t in TEMPS.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        let _ = fs::remove_file(t);
    }
    if let Some(p) = PARTIAL.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = fs::remove_dir_all(p);
    }
}

/// SIGINT/SIGTERM/SIGHUP: remove our temps and the partial backup, then exit 130/143/129 (the
/// shell tool's trap). The signals are blocked in this thread before any other exists and
/// received by a dedicated thread with `sigwait`, so the cleanup runs as ordinary code, not in a
/// signal handler. Children get a clean mask (the standard library resets it at exec).
pub fn install_signal_cleanup() {
    // SAFETY: plain libc calls on a locally owned sigset; no other thread exists yet (this runs
    // first thing in the command), so the mask is inherited by the waiter thread only.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return;
        }
        let waiter = set;
        std::thread::spawn(move || {
            let mut sig: libc::c_int = 0;
            if libc::sigwait(&waiter, &mut sig) == 0 {
                cleanup();
                libc::_exit(128 + sig);
            }
        });
    }
}

/// A test-only fault point (`INSTALL_RS_FAULT=<point>`): exit 99 there, as a kill would.
pub fn fault(configured: Option<&str>, point: &str) -> R<()> {
    if configured == Some(point) {
        say(format_args!(
            "FAULT HOOK (test-only): simulating a kill at '{point}'"
        ));
        return Err(Stop(99));
    }
    Ok(())
}

/// `<name>.tmp.<all digits>`: our temp naming (the digits are the writer's pid).
pub fn is_our_temp_name(name: &str) -> bool {
    name.contains(".tmp.")
        && name
            .rsplit_once('.')
            .is_some_and(|(_, d)| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Remove a SIGKILLed earlier run's temps of these dests (`<dest>.tmp.<digits>`, our exact
/// naming only; a foreign `x.tmp.notdigits` stays). `rels` are relative to `base`.
pub fn sweep_dest_temps(base: &Path, rels: &[&str]) {
    for rel in rels {
        sweep_one(&base.join(rel), rel);
    }
}

/// [`sweep_dest_temps`] for one absolute dest; `shown` is how the message names it.
pub fn sweep_one(dest: &Path, shown: &str) {
    let (Some(dir), Some(base)) = (dest.parent(), dest.file_name()) else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{}.tmp.", base.to_string_lossy());
    let mut hits: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix(&prefix)
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    hits.sort();
    for name in hits {
        if fs::remove_file(dir.join(&name)).is_ok() {
            let suffix = &name[prefix.len()..];
            say(format_args!(
                "  removed a killed run's stray temp: {shown}.tmp.{suffix}"
            ));
        }
    }
}

// ----- file helpers -------------------------------------------------------------------------------

pub fn sha_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Byte-equal files (`cmp -s`): false when either is unreadable.
pub fn same_bytes(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// `cp -p`: content, mode, timestamps and (when permitted) owner. A failed chown is ignored,
/// as `cp -p` does for an unprivileged user.
pub fn copy_p(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    fs::copy(src, dst)?;
    let times = FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    OpenOptions::new().write(true).open(dst)?.set_times(times)?;
    let _ = std::os::unix::fs::chown(dst, Some(meta.uid()), Some(meta.gid()));
    Ok(())
}

/// Replace a file's content in place (truncate + write), keeping its mode and owner: what
/// `cat src > tmp` does to a temp pre-seeded with `cp -p <orig>`.
pub fn write_into(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new().write(true).truncate(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// The temp for an atomic write of `dest`: `<dest>.tmp.<pid>` in the same dir, pre-seeded with a
/// `cp -p` of the original when it exists (so owner/mode survive the rename; under `podman
/// unshare` the file belongs to the container uid), registered for cleanup.
pub fn tmp_for(dest: &Path) -> io::Result<PathBuf> {
    let mut name = dest.as_os_str().to_owned();
    name.push(format!(".tmp.{}", std::process::id()));
    let t = PathBuf::from(name);
    register_temp(&t);
    if dest.exists() {
        copy_p(dest, &t)?;
    } else {
        File::create(&t)?;
    }
    Ok(t)
}

/// The fault-hooked rename that commits a temp (`point` is `pre-mv:<point>`).
pub fn commit_tmp(fault_cfg: Option<&str>, tmp: &Path, dest: &Path, point: &str) -> R<()> {
    fault(fault_cfg, &format!("pre-mv:{point}"))?;
    if let Err(e) = fs::rename(tmp, dest) {
        let _ = fs::remove_file(tmp);
        return Err(die(format_args!(
            "rename over {} failed: {e}",
            dest.display()
        )));
    }
    Ok(())
}

/// Every regular file under `dir` (not following symlinks; `find <dir> -type f`), sorted by
/// path bytes (`find | sort`). A missing dir yields nothing.
pub fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                out.push(e.path());
            }
        }
    }
    out.sort_by(|a, b| {
        a.as_os_str()
            .as_encoded_bytes()
            .cmp(b.as_os_str().as_encoded_bytes())
    });
    out
}

/// `realpath -m`: absolute, `.`/`..` resolved, symlinks of the existing prefix followed, the
/// missing rest taken lexically. The real-tree decisions compare both sides through this, so a
/// trailing slash, `..` segments or a symlink spelling of the real tree is still the real tree.
pub fn realpath_m(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut queue: std::collections::VecDeque<std::ffi::OsString> = abs
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    let mut out = PathBuf::from("/");
    let mut hops = 0;
    while let Some(c) = queue.pop_front() {
        if c == ".." {
            out.pop();
            continue;
        }
        let cand = out.join(&c);
        match fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() && hops < 40 => {
                hops += 1;
                let Ok(target) = fs::read_link(&cand) else {
                    out = cand;
                    continue;
                };
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                let parts: Vec<std::ffi::OsString> = target
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(n) => Some(n.to_owned()),
                        Component::ParentDir => Some("..".into()),
                        _ => None,
                    })
                    .collect();
                for part in parts.into_iter().rev() {
                    queue.push_front(part);
                }
            }
            _ => out = cand,
        }
    }
    out
}

/// coreutils `dirname`: the spelling-preserving parent (`a/b/` -> `a`, `a/b/x/..` -> `a/b/x`).
/// The Wine app dir is `dirname "$CS2_GAME"`, and the backups record it in exactly that form.
pub fn dirname(p: &Path) -> PathBuf {
    let s = p.as_os_str().as_encoded_bytes();
    let trimmed = {
        let mut end = s.len();
        while end > 1 && s[end - 1] == b'/' {
            end -= 1;
        }
        &s[..end]
    };
    if trimmed == b"/" {
        return PathBuf::from("/");
    }
    let Some(slash) = trimmed.iter().rposition(|&b| b == b'/') else {
        return PathBuf::from(".");
    };
    let mut end = slash;
    while end > 0 && trimmed[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return PathBuf::from("/");
    }
    // SAFETY: a prefix of an OsStr's encoded bytes ending before an ASCII '/' is valid
    PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&trimmed[..end]) })
}

/// `date -u '+%Y%m%dT%H%M%S.%NZ'`: the backup dir stamp. Lexicographic order is time order, and
/// the form matches what the shell tool wrote, so its backups and ours sort together.
pub fn utc_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}.{:09}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        now.subsec_nanos()
    )
}

/// Days since 1970-01-01 -> (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `cat file` inside `$(...)`: the content with trailing newlines stripped (how the backups'
/// APP_TREE is compared).
pub fn read_trimmed(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Some(text.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirname_matches_coreutils() {
        assert_eq!(dirname(Path::new("/a/app/game")), PathBuf::from("/a/app"));
        assert_eq!(dirname(Path::new("/a/app/game/")), PathBuf::from("/a/app"));
        assert_eq!(
            dirname(Path::new("/a/app/game/x/..")),
            PathBuf::from("/a/app/game/x")
        );
        assert_eq!(dirname(Path::new("/game")), PathBuf::from("/"));
        assert_eq!(dirname(Path::new("game")), PathBuf::from("."));
        assert_eq!(dirname(Path::new("/")), PathBuf::from("/"));
        assert_eq!(dirname(Path::new("a//b")), PathBuf::from("a"));
    }

    #[test]
    fn realpath_m_resolves_lexically_past_missing_parts() {
        assert_eq!(
            realpath_m(Path::new("/nonexistent-cs2/app/game/x/..")),
            PathBuf::from("/nonexistent-cs2/app/game")
        );
        assert_eq!(
            realpath_m(Path::new("/nonexistent-cs2/a/")),
            PathBuf::from("/nonexistent-cs2/a")
        );
        assert_eq!(realpath_m(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn our_temp_names_are_digits_only() {
        assert!(is_our_temp_name("sdkenginetools.txt.tmp.424242"));
        assert!(!is_our_temp_name("other.tmp.notdigits"));
        assert!(!is_our_temp_name("other.tmp."));
        assert!(!is_our_temp_name("plain.txt"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_762), (2026, 11, 5));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
