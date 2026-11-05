//! The file protocol: reading `request.json`, writing `<id>.png` / `<id>.json` atomically, and PNG
//! encoding. Shared contract with `crates/cs2-mcp-common/src/capture/vklayer.rs`.
use serde_json::{json, Value};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

pub const REQUEST_FILE: &str = "request.json";

pub struct Request {
    pub id: String,
    pub format: String,
}

pub struct FrameMeta<'a> {
    pub id: &'a str,
    pub width: u32,
    pub height: u32,
    pub format: &'a str,
    pub present_index: u64,
    pub ms: u64,
}

/// Ids name files, so they are restricted to a safe alphabet (the server applies the same rule).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `Ok(None)` when there is no request; `Err` when a request exists but cannot be parsed.
pub fn read_request(dir: &Path) -> Result<Option<Request>, String> {
    let bytes = match std::fs::read(dir.join(REQUEST_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {REQUEST_FILE}: {e}")),
    };
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("parse request: {e}"))?;
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or("request has no string id")?
        .to_owned();
    let format = v
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("png")
        .to_owned();
    Ok(Some(Request { id, format }))
}

/// Take `request.json` under a private name, then check its id. The take is a rename, so a request
/// the server publishes meanwhile can never be deleted by mistake: a request that is not ours is
/// linked back (exclusively) and only the private copy is removed.
fn take_private(dir: &Path) -> Option<std::path::PathBuf> {
    static TAKE: AtomicU64 = AtomicU64::new(0);
    let seq = TAKE.fetch_add(1, Ordering::Relaxed);
    let private = dir.join(format!(".take.{}.{seq}.tmp", std::process::id()));
    match std::fs::rename(dir.join(REQUEST_FILE), &private) {
        Ok(()) => Some(private),
        Err(_) => None,
    }
}

/// Put a taken request back unless a newer request already took its place.
fn restore(dir: &Path, private: &Path) {
    if let Err(e) = std::fs::hard_link(private, dir.join(REQUEST_FILE)) {
        // a newer request is already there (or the directory changed): this one is not restored
        dlog!("request not restored to {} ({e})", dir.display());
    }
    let _ = std::fs::remove_file(private);
}

/// Remove `request.json` only if it still names `id`.
pub fn consume(dir: &Path, id: &str) {
    let Some(private) = take_private(dir) else {
        return;
    };
    let names_id = std::fs::read(&private)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v.get("id").and_then(Value::as_str) == Some(id));
    if names_id {
        let _ = std::fs::remove_file(&private);
    } else {
        restore(dir, &private);
    }
}

/// Remove an unparseable request so it does not block the next one.
pub fn remove_request(dir: &Path) {
    if let Some(private) = take_private(dir) {
        let _ = std::fs::remove_file(private);
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Temp file in the same directory, created exclusively and without following symlinks, then
/// renamed into place: readers never see a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("output path has no file name")?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    // Windows: create_new (CREATE_NEW) fails on any existing name, a symlink included, and the
    // file inherits the capture dir's ACL
    #[cfg(unix)]
    opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = opts
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let written = file.write_all(bytes);
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename to {}: {e}", path.display())
    })
}

pub fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        w.write_image_data(rgb)
            .map_err(|e| format!("png data: {e}"))?;
        w.finish().map_err(|e| format!("png finish: {e}"))?;
    }
    Ok(buf)
}

/// The executable path of this process, for the result metadata.
fn exe() -> String {
    static EXE: LazyLock<String> = LazyLock::new(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    EXE.clone()
}

/// PNG first, then the metadata json (the server waits for the json), then the request is consumed.
pub fn publish_frame(dir: &Path, meta: &FrameMeta, rgb: &[u8]) -> Result<(), String> {
    let png = encode_png(meta.width, meta.height, rgb)?;
    write_atomic(&dir.join(format!("{}.png", meta.id)), &png)?;
    let body = json!({
        "id": meta.id,
        "width": meta.width,
        "height": meta.height,
        "format": meta.format,
        "presentIndex": meta.present_index,
        "ms": meta.ms,
        "pid": std::process::id(),
        "exe": exe(),
    });
    write_atomic(
        &dir.join(format!("{}.json", meta.id)),
        body.to_string().as_bytes(),
    )?;
    consume(dir, meta.id);
    Ok(())
}

/// `{id, error, pid, exe}` in place of a frame. Also consumes the request.
pub fn publish_error(dir: &Path, id: &str, message: &str) {
    let body = json!({
        "id": id,
        "error": message,
        "pid": std::process::id(),
        "exe": exe(),
    });
    if let Err(e) = write_atomic(&dir.join(format!("{id}.json")), body.to_string().as_bytes()) {
        dlog!("cannot write error for {id}: {e}");
    }
    dlog!("capture {id} failed: {message}");
    consume(dir, id);
}
