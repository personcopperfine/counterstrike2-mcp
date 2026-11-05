//! The only `.vmdl` file writer: back up, write a temporary, rename atomically.
//!
//! A `.vmdl` the editor may have open is never truncated in place: the new text goes to a fresh
//! `<name>.vmdl.tmp-<pid>-<n>` next to the target and is `rename`d over it (atomic on one
//! filesystem), so a reader either sees the old file or the new one, never a half-written model.
//! When the target exists it is first copied to `<name>.vmdl.bak-<unix seconds>`, or
//! `.bak-<seconds>-<k>` when that name is taken: every write keeps its own backup (two writes in
//! one second must not overwrite the backup of the original). Both side files are created with
//! `create_new`, so an existing file or symlink of that name is never followed or truncated.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process counter for temporary names (concurrent writes in one process never share one).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Create a new sibling file `<name><separator><tag>[-k]` (first free `k`) and return it open.
fn create_sibling(path: &Path, separator: &str, tag: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    for k in 0..1000u32 {
        let candidate = if k == 0 {
            sibling(path, separator, tag)
        } else {
            sibling(path, separator, &format!("{tag}-{k}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free {separator}{tag}-<k> name next to {}", path.display()),
    ))
}

/// Write `text` to `path` atomically, returning the backup path when the target already existed.
///
/// The parent directory must exist (see
/// [`ContentPaths::resolve_write`](crate::vmdl::ContentPaths::resolve_write)); a failed write
/// leaves no `.tmp-` file behind.
pub fn write_atomic_with_backup(path: &Path, text: &str) -> std::io::Result<Option<PathBuf>> {
    write_atomic_bytes_with_backup(path, text.as_bytes())
}

/// [`write_atomic_with_backup`] for binary content (a `.vmap` is binary DMX).
pub fn write_atomic_bytes_with_backup(path: &Path, bytes: &[u8]) -> std::io::Result<Option<PathBuf>> {
    let backup = if path.exists() {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let original = std::fs::read(path)?;
        let (backup, mut file) = create_sibling(path, ".bak-", &seconds.to_string())?;
        if let Err(error) = file.write_all(&original).and_then(|_| file.sync_all()) {
            let _ = std::fs::remove_file(&backup);
            return Err(error);
        }
        Some(backup)
    } else {
        None
    };
    let tag = format!("{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed));
    let (temporary, mut file) = create_sibling(path, ".tmp-", &tag)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(backup)
}

/// Publish `bytes` as a NEW file at `path`, atomically and without ever replacing anything: the
/// content goes to a temporary sibling, which is then hard-linked to `path` (`link` fails with
/// `AlreadyExists` when `path` exists, even if another writer created it a moment ago) and the
/// temporary is removed. A filesystem without hard links reports the error as is.
pub fn write_atomic_bytes_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tag = format!("{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed));
    let (temporary, mut file) = create_sibling(path, ".tmp-", &tag)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    let linked = std::fs::hard_link(&temporary, path);
    let _ = std::fs::remove_file(&temporary);
    linked
}

/// `path` with `suffix + tag` appended to the whole file name (`x.vmdl` -> `x.vmdl.bak-17`).
fn sibling(path: &Path, separator: &str, tag: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(separator);
    name.push(tag);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}
