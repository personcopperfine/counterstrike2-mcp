//! Server side of the `cs2-vk-capture` Vulkan layer protocol (layer and `README.md` in
//! `../../../cs2-vk-capture`). `request_frame` publishes `<dir>/request.json` exclusively, waits for
//! the layer's `<id>.json` (and `<id>.png`), and returns the PNG path. The MCP tools (`screenshot`,
//! `observe`) call it from client-mcp.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub const REQUEST_FILE: &str = "request.json";
/// Files a built layer directory must contain (`client-mcp/docker/build-vk-layer.sh` copies them).
pub const LAYER_LIBRARY: &str = "libcs2_vk_capture.so";
pub const LAYER_MANIFEST: &str = "VkLayer_cs2mcp_capture.json";
/// Name the explicit layer is enabled by (`VK_INSTANCE_LAYERS`).
pub const LAYER_NAME: &str = "VK_LAYER_CS2MCP_capture";
/// Where a container sees the layer directory (read-only) and the capture dir (`<artifacts>/capture`
/// under the `/work` mount). Container side only; the host side is chosen by the runtime.
pub const CONTAINER_LAYER_DIR: &str = "/cs2mcp/vk";
pub const CONTAINER_CAPTURE_DIR: &str = "/work/capture";
const POLL: Duration = Duration::from_millis(20);
/// A request this old (by mtime) was never taken by a layer; it is removed rather than waited on.
pub const STALE_AFTER: Duration = Duration::from_secs(10);

/// One request at a time per process: two callers never race for `request.json`.
/// One lock per capture directory: a directory holds one `request.json` at a time, but two sessions
/// (two directories) never wait for each other.
static REQUEST_LOCKS: std::sync::Mutex<Option<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::Mutex::new(None);

fn dir_lock(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = REQUEST_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.get_or_insert_with(HashMap::new)
        .entry(dir.to_path_buf())
        .or_default()
        .clone()
}
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Same alphabet the layer accepts: ids become file names.
pub fn valid_request_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why a frame request failed. `Timeout`: no answer from a layer (not loaded, not presenting, or
/// not reading the directory); `LayerFailed`: the layer answered with `{error}`; `Setup`: the
/// request could not be made at all (bad id, unsafe directory, I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameErrorKind {
    Timeout,
    LayerFailed,
    Setup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub kind: FrameErrorKind,
    pub message: String,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FrameError {}

fn setup(e: anyhow::Error) -> FrameError {
    FrameError {
        kind: FrameErrorKind::Setup,
        message: format!("{e:#}"),
    }
}

/// Ask the game's Vulkan layer for the next presented frame and wait for it.
///
/// Returns `<dir>/<id>.png`; the metadata is in `<dir>/<id>.json`
/// (`{id, width, height, format, presentIndex, ms, pid, exe}`). A layer-side failure is
/// [`FrameErrorKind::LayerFailed`] carrying the layer's message. On timeout the pending request is
/// withdrawn, so it cannot fire later.
pub async fn request_frame(dir: &Path, id: &str, timeout: Duration) -> Result<PathBuf, FrameError> {
    if !valid_request_id(id) {
        return Err(setup(anyhow::anyhow!(
            "invalid capture id {id:?}: use 1-64 letters, digits, '-' or '_'"
        )));
    }
    let deadline = Instant::now() + timeout;
    // the wait for another request in this directory counts against the same deadline
    let lock = dir_lock(dir);
    let _slot = match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), lock.lock())
        .await
    {
        Ok(guard) => guard,
        Err(_) => {
            return Err(FrameError {
                kind: FrameErrorKind::Timeout,
                message: format!(
                    "timed out after {timeout:?} waiting for an earlier capture in {}",
                    dir.display()
                ),
            })
        }
    };
    create_private_dir(dir).map_err(setup)?;
    wait_for_slot(dir, deadline)
        .await
        .map_err(|e| match e.downcast::<FrameError>() {
            Ok(timeout) => timeout,
            Err(other) => setup(other),
        })?;

    let json_path = dir.join(format!("{id}.json"));
    let png_path = dir.join(format!("{id}.png"));
    let _ = std::fs::remove_file(&json_path);
    let _ = std::fs::remove_file(&png_path);
    publish_request(dir, id).map_err(setup)?;

    loop {
        if json_path.exists() {
            let bytes = std::fs::read(&json_path)
                .with_context(|| format!("read {}", json_path.display()))
                .map_err(setup)?;
            let meta: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", json_path.display()))
                .map_err(setup)?;
            if let Some(err) = meta.get("error") {
                return Err(FrameError {
                    kind: FrameErrorKind::LayerFailed,
                    message: format!("vulkan capture failed: {err}"),
                });
            }
            return Ok(png_path);
        }
        if Instant::now() >= deadline {
            withdraw(dir, id);
            return Err(FrameError {
                kind: FrameErrorKind::Timeout,
                message: format!(
                    "timed out after {timeout:?} waiting for a presented frame \
                     (is {LAYER_NAME} loaded, and is the game presenting?)"
                ),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Which files a layer directory holds. `valid` only when both the library and the manifest are
/// present (a symlink to either is fine: the check follows links).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerStatus {
    pub dir: PathBuf,
    pub valid: bool,
    pub missing: Vec<&'static str>,
}

impl LayerStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "dir": self.dir.display().to_string(),
            "valid": self.valid,
            "missing": self.missing,
            "layer": LAYER_NAME,
        })
    }
}

pub fn layer_status(dir: &Path) -> LayerStatus {
    let missing: Vec<&'static str> = [LAYER_LIBRARY, LAYER_MANIFEST]
        .into_iter()
        .filter(|f| !dir.join(f).is_file())
        .collect();
    LayerStatus {
        dir: dir.to_path_buf(),
        valid: missing.is_empty(),
        missing,
    }
}

/// Create the capture directory if needed (0700), then check that it is a real directory owned by
/// the effective user with no group or other bits. Anything else is refused: a request in a shared
/// or symlinked directory could be read or planted by someone else.
fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(dir)
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(e)
            }
        })
        .with_context(|| format!("create {}", dir.display()))?;
    let m = std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    if !m.is_dir() || m.file_type().is_symlink() {
        bail!("capture dir {} is not a real directory", dir.display());
    }
    #[cfg(unix)]
    check_private_unix(dir, &m)?;
    // Windows: no reparse point and owned by this user (the layer checks the same); the DACL is
    // not checked (a dir under %LOCALAPPDATA% inherits the profile's private one)
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("capture dir {} is not a real directory (a reparse point)", dir.display());
        }
        if !super::winsec::owned_by_me(dir) {
            bail!("capture dir {} is not owned by this user", dir.display());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn check_private_unix(dir: &Path, m: &std::fs::Metadata) -> Result<()> {
    // SAFETY: geteuid cannot fail.
    let uid = unsafe { libc::geteuid() };
    if m.uid() != uid {
        bail!("capture dir {} is not owned by this user", dir.display());
    }
    if m.mode() & 0o077 != 0 {
        bail!("capture dir {} must be private (mode 0700)", dir.display());
    }
    Ok(())
}

/// Take `request.json` under a private name (a rename, so a request published meanwhile is never
/// deleted by mistake). Returns the private path, or `None` if there was no request.
fn take_private(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let private = dir.join(format!(".take.{}.{seq}.tmp", std::process::id()));
    match std::fs::rename(dir.join(REQUEST_FILE), &private) {
        Ok(()) => Ok(Some(private)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Put a taken request back unless a newer one took its place.
fn restore(dir: &Path, private: &Path) {
    if let Err(e) = std::fs::hard_link(private, dir.join(REQUEST_FILE)) {
        tracing::warn!("capture request not restored in {} ({e})", dir.display());
    }
    let _ = std::fs::remove_file(private);
}

fn age_of(path: &Path) -> Duration {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default()
}

/// Wait until no request is pending. A request older than [`STALE_AFTER`] is taken and removed.
async fn wait_for_slot(dir: &Path, deadline: Instant) -> Result<()> {
    let path = dir.join(REQUEST_FILE);
    loop {
        match std::fs::metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
            Ok(meta) => {
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .unwrap_or_default();
                if age > STALE_AFTER {
                    if let Some(private) = take_private(dir)? {
                        if age_of(&private) > STALE_AFTER {
                            let _ = std::fs::remove_file(&private);
                        } else {
                            restore(dir, &private);
                        }
                    }
                    continue;
                }
            }
        }
        if Instant::now() >= deadline {
            // the layer is not taking requests: a timeout, not a setup failure
            return Err(anyhow::Error::new(FrameError {
                kind: FrameErrorKind::Timeout,
                message: format!(
                    "an earlier capture request is still pending in {}",
                    dir.display()
                ),
            }));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Publish `request.json` with a hard link from a private temp file. The link fails if the file
/// exists, so an existing request is never overwritten.
fn publish_request(dir: &Path, id: &str) -> Result<()> {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".request.{}.{seq}.tmp", std::process::id()));
    let body = json!({ "id": id, "format": "png" }).to_string();
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
    }
    let linked = std::fs::hard_link(&tmp, dir.join(REQUEST_FILE));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("an earlier capture request appeared while publishing")
        }
        Err(e) => Err(e).context("publish request.json"),
    }
}

/// Remove `request.json` if it still names `id`; any other request is put back.
fn withdraw(dir: &Path, id: &str) {
    let Ok(Some(private)) = take_private(dir) else {
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

/// Delete the result files (`<id>.png`, `<id>.json`) of all but the `keep_last` most recent ids
/// (by mtime). Returns the number of ids removed. The request file and temp files are never
/// touched.
pub async fn cleanup(dir: &Path, keep_last: usize) -> Result<usize> {
    let mut newest: HashMap<String, SystemTime> = HashMap::new();
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("read {}", dir.display()))?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == REQUEST_FILE {
            continue;
        }
        let Some(id) = name
            .strip_suffix(".json")
            .or_else(|| name.strip_suffix(".png"))
        else {
            continue;
        };
        if !valid_request_id(id) {
            continue;
        }
        let mtime = entry.metadata().await?.modified()?;
        let slot = newest.entry(id.to_owned()).or_insert(mtime);
        if mtime > *slot {
            *slot = mtime;
        }
    }
    let mut ids: Vec<(String, SystemTime)> = newest.into_iter().collect();
    ids.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut removed = 0;
    for (id, _) in ids.into_iter().skip(keep_last) {
        for ext in ["json", "png"] {
            let _ = tokio::fs::remove_file(dir.join(format!("{id}.{ext}"))).await;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/vk"));
        let d = root
            .join("test-scratch")
            .join(format!("vklayer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A 0700 directory, as the server must create it.
    fn private_dir(dir: &Path) {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .unwrap();
    }

    #[tokio::test]
    async fn a_shared_capture_dir_is_refused() {
        let dir = scratch("shared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let err = request_frame(&dir, "s1", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("0700"), "{err}");
        assert!(!dir.join(REQUEST_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ids_are_filename_safe() {
        assert!(valid_request_id("shot-1_a"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("../x"));
        assert!(!valid_request_id("a b"));
        assert!(!valid_request_id(&"a".repeat(65)));
    }

    #[tokio::test]
    async fn round_trip_with_a_fake_layer() {
        let dir = scratch("ok");
        let d2 = dir.clone();
        // stand-in for the layer: answer once the request is out, then take it
        let layer = tokio::spawn(async move {
            loop {
                if let Ok(bytes) = std::fs::read(d2.join(REQUEST_FILE)) {
                    let v: Value = serde_json::from_slice(&bytes).unwrap();
                    let id = v["id"].as_str().unwrap().to_owned();
                    std::fs::write(d2.join(format!("{id}.png")), b"png").unwrap();
                    std::fs::write(
                        d2.join(format!("{id}.json")),
                        json!({"id": id, "width": 1, "height": 1}).to_string(),
                    )
                    .unwrap();
                    std::fs::remove_file(d2.join(REQUEST_FILE)).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let png = request_frame(&dir, "t1", Duration::from_secs(5))
            .await
            .unwrap();
        layer.await.unwrap();
        assert_eq!(png, dir.join("t1.png"));
        assert_eq!(std::fs::read(&png).unwrap(), b"png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn layer_error_is_reported() {
        let dir = scratch("err");
        private_dir(&dir);
        let d2 = dir.clone();
        let layer = tokio::spawn(async move {
            while !d2.join(REQUEST_FILE).exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            std::fs::write(
                d2.join("e1.json"),
                json!({"id": "e1", "error": "no swapchain"}).to_string(),
            )
            .unwrap();
            std::fs::remove_file(d2.join(REQUEST_FILE)).unwrap();
        });
        let err = request_frame(&dir, "e1", Duration::from_secs(5))
            .await
            .unwrap_err();
        layer.await.unwrap();
        assert!(err.to_string().contains("no swapchain"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_withdraws_the_request() {
        let dir = scratch("timeout");
        let err = request_frame(&dir, "t2", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(!dir.join(REQUEST_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_request_is_removed_not_waited_on() {
        let dir = scratch("stale");
        private_dir(&dir);
        std::fs::write(dir.join(REQUEST_FILE), r#"{"id":"old","format":"png"}"#).unwrap();
        let past = SystemTime::now() - STALE_AFTER - Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(dir.join(REQUEST_FILE))
            .unwrap()
            .set_modified(past)
            .unwrap();
        let d2 = dir.clone();
        let layer = tokio::spawn(async move {
            loop {
                if let Ok(bytes) = std::fs::read(d2.join(REQUEST_FILE)) {
                    let v: Value = serde_json::from_slice(&bytes).unwrap();
                    if v["id"] == "fresh" {
                        std::fs::write(d2.join("fresh.png"), b"x").unwrap();
                        std::fs::write(d2.join("fresh.json"), b"{\"id\":\"fresh\"}").unwrap();
                        std::fs::remove_file(d2.join(REQUEST_FILE)).unwrap();
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        request_frame(&dir, "fresh", Duration::from_secs(5))
            .await
            .unwrap();
        layer.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cleanup_keeps_the_newest_ids() {
        let dir = scratch("cleanup");
        private_dir(&dir);
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            std::fs::write(dir.join(format!("{id}.json")), "{}").unwrap();
            std::fs::write(dir.join(format!("{id}.png")), "p").unwrap();
            let t = SystemTime::now() - Duration::from_secs(100 - i as u64 * 10);
            for ext in ["json", "png"] {
                std::fs::File::options()
                    .write(true)
                    .open(dir.join(format!("{id}.{ext}")))
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        }
        let removed = cleanup(&dir, 2).await.unwrap();
        assert_eq!(removed, 1);
        assert!(!dir.join("a.json").exists() && !dir.join("a.png").exists());
        assert!(dir.join("b.png").exists() && dir.join("c.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
