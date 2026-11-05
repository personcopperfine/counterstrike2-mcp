//! Text extraction from captures. Phase 1: shell out to `tesseract` (present on this host at
//! /usr/bin/tesseract; add it to our own image under ./docker). Native bindings (`tesseract` /
//! `tesseract-rs` crates) are a later option behind a feature flag.
//!
//! Errors are typed by where they came from, so the tool layer can name them correctly:
//! [`OcrError::Invalid`] (bad crop or psm: the caller's arguments), [`OcrError::Unreadable`] (the
//! input is missing, not a PNG, or too large: a capture problem), and [`OcrError::Engine`] (the
//! tesseract process is missing, failed, or timed out).
//!
//! Every tesseract run has a hard timeout and kills the child on drop. File work (decode, crop,
//! upscale, temp write) runs on the blocking pool.
use super::frame::{check_dims, load_png_rgb};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ENGINE: &str = "tesseract-cli";
pub const TESSERACT: &str = "tesseract";
/// Ceiling for one OCR run; the tool further caps it by the call's remaining budget.
pub const OCR_TIMEOUT: Duration = Duration::from_secs(60);
/// Upscale factor applied to a crop before OCR (small HUD text is unreadable at 1x).
pub const CROP_UPSCALE: u32 = 2;

#[derive(Debug, Clone)]
pub struct OcrResult {
    pub text: String,
    /// `tesseract --psm` used, for reproducibility
    pub psm: u8,
    pub engine: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The caller's arguments are wrong (crop outside the image, psm out of range).
    #[error("{0}")]
    Invalid(String),
    /// The input cannot be read as a PNG frame (missing, not a PNG, too large).
    #[error("{0}")]
    Unreadable(String),
    /// tesseract is missing, exited non-zero, or timed out.
    #[error("{0}")]
    Engine(String),
}

/// OCR a whole PNG file.
pub async fn ocr_png(png: &Path, psm: u8, timeout: Duration) -> Result<OcrResult, OcrError> {
    let owned = png.to_path_buf();
    blocking(move || check_png_file(&owned)).await?;
    run_tesseract(TESSERACT, png, psm, timeout).await
}

/// OCR a crop `[x, y, w, h]` of a PNG, upscaled [`CROP_UPSCALE`]x with nearest-neighbour.
pub async fn ocr_region(
    png: &Path,
    crop: [u32; 4],
    psm: u8,
    timeout: Duration,
) -> Result<OcrResult, OcrError> {
    let owned = png.to_path_buf();
    // the temp file is removed when `tmp` drops, on every path out of this function
    let tmp = blocking(move || prepare_region(&owned, crop)).await?;
    run_tesseract(TESSERACT, tmp.path(), psm, timeout).await
}

/// True when the `tesseract` binary can be executed.
pub async fn tesseract_available() -> bool {
    tokio::process::Command::new(TESSERACT)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn blocking<T, F>(f: F) -> Result<T, OcrError>
where
    F: FnOnce() -> Result<T, OcrError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| OcrError::Engine(format!("blocking task failed: {e}")))?
}

/// Header-only check: the file opens and is a PNG within the frame size cap.
fn check_png_file(path: &Path) -> Result<(), OcrError> {
    let file = std::fs::File::open(path)
        .map_err(|e| OcrError::Unreadable(format!("open {}: {e}", path.display())))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let reader = decoder.read_info().map_err(|e| {
        OcrError::Unreadable(format!("{} is not a readable PNG: {e}", path.display()))
    })?;
    let info = reader.info();
    check_dims(info.width, info.height).map_err(|e| OcrError::Unreadable(format!("{e:#}")))
}

fn prepare_region(png: &Path, crop: [u32; 4]) -> Result<TempPng, OcrError> {
    let [x, y, w, h] = crop;
    let img = load_png_rgb(png).map_err(|e| OcrError::Unreadable(format!("{e:#}")))?;
    let region = img
        .crop(x, y, w, h)
        .map_err(|e| OcrError::Invalid(format!("{e:#}")))?
        .upscale(CROP_UPSCALE)
        .map_err(|e| OcrError::Invalid(format!("{e:#}")))?;
    let bytes = region
        .to_png()
        .map_err(|e| OcrError::Unreadable(format!("encode crop: {e:#}")))?;
    TempPng::create(&bytes)
}

async fn run_tesseract(
    bin: &str,
    png: &Path,
    psm: u8,
    timeout: Duration,
) -> Result<OcrResult, OcrError> {
    if psm > 13 {
        return Err(OcrError::Invalid(format!(
            "tesseract psm must be 0..=13, got {psm}"
        )));
    }
    let args = [
        png.as_os_str().to_owned(),
        "stdout".into(),
        "--psm".into(),
        psm.to_string().into(),
    ];
    let out = run_with_timeout(bin, &args, timeout).await?;
    Ok(OcrResult {
        text: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        psm,
        engine: ENGINE,
    })
}

/// Run `bin args...` with a hard timeout; the child is killed if the timeout drops the future.
async fn run_with_timeout(
    bin: &str,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> Result<std::process::Output, OcrError> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OcrError::Engine(format!(
                "tesseract not found ({bin}); install tesseract-ocr on the host or in the image (workflows/09-vision-ocr.md)"
            ))
        } else {
            OcrError::Engine(format!("spawn {bin}: {e}"))
        }
    })?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => r.map_err(|e| OcrError::Engine(format!("tesseract run: {e}")))?,
        Err(_) => {
            return Err(OcrError::Engine(format!(
                "tesseract timed out after {} ms",
                timeout.as_millis()
            )))
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(OcrError::Engine(format!(
            "tesseract exited with {}: {}",
            out.status,
            err.trim()
        )));
    }
    Ok(out)
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A PNG in the temp directory, deleted when dropped. Created with `create_new` under a name
/// that includes pid, wall-clock nanoseconds and a counter, so a pre-planted file is never reused.
struct TempPng {
    path: PathBuf,
}

impl TempPng {
    fn create(bytes: &[u8]) -> Result<TempPng, OcrError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "cs2mcp-ocr-{}-{nanos}-{}.png",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| OcrError::Engine(format!("create temp {}: {e}", path.display())))?;
        // guard first: a failed write still removes the partial file
        let guard = TempPng { path };
        file.write_all(bytes)
            .map_err(|e| OcrError::Engine(format!("write temp {}: {e}", guard.path.display())))?;
        Ok(guard)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPng {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::frame::RgbImage;

    /// Golden fixture: `HEALTH 100` in Lato-Medium 48 pt, black on white, 400x100. Generated once
    /// with Pillow (`ImageDraw.text((10, 20), ...)`); the digits sit at x ~ 205..290.
    const HEALTH_FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/ocr/health_100.png");

    fn write_white(dir: &Path, w: u32, h: u32) -> PathBuf {
        let p = dir.join(format!("white-{w}x{h}.png"));
        let img = RgbImage::new(w, h, vec![255; (w * h * 3) as usize]).unwrap();
        std::fs::write(&p, img.to_png().unwrap()).unwrap();
        p
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cs2mcp-ocr-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const T: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn golden_health_fixture_reads_100() {
        if !tesseract_available().await {
            eprintln!("skipping: tesseract not installed");
            return;
        }
        let r = ocr_png(Path::new(HEALTH_FIXTURE), 6, T).await.unwrap();
        assert!(r.text.contains("100"), "OCR read {:?}", r.text);
        assert!(r.text.contains("HEALTH"), "OCR read {:?}", r.text);
        // the same digits through a crop around the number (upscaled before OCR)
        let region = ocr_region(Path::new(HEALTH_FIXTURE), [195, 0, 120, 100], 6, T)
            .await
            .unwrap();
        assert!(region.text.contains("100"), "crop read {:?}", region.text);
    }

    #[tokio::test]
    async fn white_image_has_no_text() {
        if !tesseract_available().await {
            eprintln!("skipping: tesseract not installed");
            return;
        }
        let dir = scratch("white");
        let png = write_white(&dir, 200, 80);
        let r = ocr_png(&png, 6, T).await.unwrap();
        assert_eq!(r.psm, 6);
        assert_eq!(r.engine, ENGINE);
        assert!(r.text.trim().is_empty(), "unexpected text: {:?}", r.text);
        let region = ocr_region(&png, [10, 10, 100, 40], 11, T).await.unwrap();
        assert!(region.text.trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn out_of_bounds_crop_is_invalid_and_bad_file_is_unreadable() {
        let dir = scratch("errors");
        let png = write_white(&dir, 50, 50);
        let err = ocr_region(&png, [40, 40, 30, 30], 6, T).await.unwrap_err();
        assert!(matches!(err, OcrError::Invalid(_)), "{err:?}");
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"definitely not a png").unwrap();
        let err = ocr_png(&junk, 6, T).await.unwrap_err();
        assert!(matches!(err, OcrError::Unreadable(_)), "{err:?}");
        let err = ocr_region(&junk, [0, 0, 5, 5], 6, T).await.unwrap_err();
        assert!(matches!(err, OcrError::Unreadable(_)), "{err:?}");
        let err = ocr_png(&dir.join("missing.png"), 6, T).await.unwrap_err();
        assert!(matches!(err, OcrError::Unreadable(_)), "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_binary_is_an_engine_error() {
        let dir = scratch("missing");
        let png = write_white(&dir, 8, 8);
        let args = [png.as_os_str().to_owned()];
        let err = run_with_timeout("/nonexistent/cs2mcp-tesseract", &args, T)
            .await
            .unwrap_err();
        assert!(
            matches!(err, OcrError::Engine(ref m) if m.contains("tesseract not found")),
            "{err:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_is_enforced() {
        let err = run_with_timeout("sleep", &["5".into()], Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OcrError::Engine(ref m) if m.contains("timed out")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_bad_psm_before_spawning() {
        let err = run_tesseract("/nonexistent/x", Path::new("/nonexistent.png"), 14, T)
            .await
            .unwrap_err();
        assert!(matches!(err, OcrError::Invalid(_)));
    }

    #[test]
    fn temp_png_is_removed_on_drop_and_never_reused() {
        let a = TempPng::create(b"one").unwrap();
        let b = TempPng::create(b"two").unwrap();
        assert_ne!(a.path(), b.path());
        let pa = a.path().to_path_buf();
        assert_eq!(std::fs::read(&pa).unwrap(), b"one");
        drop(a);
        assert!(!pa.exists());
        // create_new: a planted file at the next name is refused, not overwritten
        let planted =
            std::env::temp_dir().join(format!("cs2mcp-ocr-planted-{}", std::process::id()));
        std::fs::write(&planted, b"keep").unwrap();
        let opened = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&planted);
        assert!(opened.is_err());
        assert_eq!(std::fs::read(&planted).unwrap(), b"keep");
        let _ = std::fs::remove_file(&planted);
    }
}
