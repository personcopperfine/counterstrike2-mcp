//! Live tests: the layer inside a real Vulkan loader, presenting through a headless swapchain.
//! They need a Vulkan ICD (default lavapipe) and a built layer; run with
//!
//! ```text
//! CARGO_TARGET_DIR=<repo>/target/vk cargo build -p cs2-vk-capture
//! CARGO_TARGET_DIR=<repo>/target/vk cargo test -p cs2-vk-capture --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `CS2VK_ICD` picks the driver. `CS2VK_VALIDATION_DIR` (a dir with a `VkLayer_khronos_validation`
//! manifest whose `library_path` is `./...so`) adds validation and synchronization validation to
//! the runs that support it; any VUID or SYNC message fails them.
use cs2_mcp_common::capture::frame::load_png_rgb;
use cs2_mcp_common::capture::vklayer::request_frame;
use cs2_vk_capture::testpattern;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `CARGO_TARGET_DIR` if set (the repo uses `<repo>/target/vk`), else `<crate>/../../target/vk`.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/vk"))
}

fn layer_so() -> PathBuf {
    let so = target_dir().join("debug/libcs2_vk_capture.so");
    assert!(
        so.exists(),
        "missing {} - run `cargo build -p cs2-vk-capture` first",
        so.display()
    );
    so
}

fn icd() -> String {
    std::env::var("CS2VK_ICD").unwrap_or_else(|_| "/usr/share/vulkan/icd.d/lvp_icd.json".into())
}

fn validation_dir() -> Option<PathBuf> {
    std::env::var_os("CS2VK_VALIDATION_DIR").map(PathBuf::from)
}

/// A fresh scratch directory for one test, under the target dir.
fn scratch(tag: &str) -> PathBuf {
    let d = target_dir()
        .join("test-scratch")
        .join(format!("live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 0700 directory, as the layer requires for the capture directory.
fn private_dir(dir: &Path) {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .unwrap();
}

/// The layer manifest and library, side by side, as the loader expects them. Returns the value
/// for `VK_ADD_LAYER_PATH` (plus the validation directory when enabled).
fn layer_path(base: &Path) -> String {
    let dir = base.join("layers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("VkLayer_cs2mcp_capture.json"),
        dir.join("VkLayer_cs2mcp_capture.json"),
    )
    .unwrap();
    let link = dir.join("libcs2_vk_capture.so");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(layer_so(), &link).unwrap();
    match validation_dir() {
        Some(v) => format!("{}:{}", dir.display(), v.display()),
        None => dir.display().to_string(),
    }
}

fn instance_layers() -> &'static str {
    if validation_dir().is_some() {
        "VK_LAYER_CS2MCP_capture:VK_LAYER_KHRONOS_validation"
    } else {
        "VK_LAYER_CS2MCP_capture"
    }
}

/// Fail on any validation error in a log; require the loader to have inserted validation when it
/// was asked for (a missing layer must not look like "no errors").
fn check_validation(log: &Path) {
    if validation_dir().is_none() {
        return;
    }
    let text = std::fs::read_to_string(log).unwrap_or_default();
    assert!(
        text.contains("Insert instance layer \"VK_LAYER_KHRONOS_validation\""),
        "validation layer was not loaded"
    );
    let bad: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("VUID") || l.contains("SYNC-") || l.contains("Validation Error"))
        .collect();
    assert!(bad.is_empty(), "validation errors:\n{}", bad.join("\n"));
    println!("validation: loaded, no VUID or SYNC errors");
}

/// A running harness with its output in `log`.
struct Harness {
    child: Child,
    log: PathBuf,
}

impl Harness {
    /// Wait for the harness to exit by itself (it was told to stop); kill it after `timeout`.
    fn finish(mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("harness did not exit after the stop file");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Start the headless harness in continuous mode, with the layer and (optionally) validation.
fn spawn_harness(base: &Path, cap: &Path, extra_env: &[(&str, &str)]) -> Harness {
    let log = base.join("harness.log");
    let out = std::fs::File::create(&log).unwrap();
    let err = out.try_clone().unwrap();
    let stop = base.join("stop");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vk_present_harness"));
    cmd.args([
        "--no-schedule",
        "--frames",
        "100000",
        "--pace-ms",
        "2",
        "--stop-file",
    ])
    .arg(&stop)
    .env("VK_DRIVER_FILES", icd())
    .env("VK_ADD_LAYER_PATH", layer_path(base))
    .env("VK_INSTANCE_LAYERS", instance_layers())
    .env("CS2MCP_CAPTURE_DIR", cap)
    .env("CS2MCP_CAPTURE_DEBUG", "1")
    .stdout(Stdio::from(out))
    .stderr(Stdio::from(err));
    if validation_dir().is_some() {
        cmd.env("VK_LAYER_VALIDATE_SYNC", "1");
        cmd.env("VK_LOADER_DEBUG", "layer");
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    Harness {
        child: cmd.spawn().expect("spawn harness"),
        log,
    }
}

#[test]
#[ignore = "needs a Vulkan ICD and a built layer; see the module docs"]
fn harness_schedule_is_pixel_exact() {
    let base = scratch("schedule");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vkcube.sh");
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .env("CS2VK_LAYER_SO", layer_so())
        .env("CS2VK_HARNESS", env!("CARGO_BIN_EXE_vk_present_harness"))
        .env("CS2VK_ICD", icd())
        .env("CS2VK_WORK", &base)
        .env("CARGO_TARGET_DIR", target_dir());
    if let Some(v) = validation_dir() {
        cmd.env("CS2VK_VALIDATION_DIR", v);
    }
    let output = cmd.output().expect("run vkcube.sh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");
    println!(
        "--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.code() == Some(3) {
        println!("SKIPPED: the ICD has no headless surface");
        return;
    }
    assert!(output.status.success(), "vkcube.sh failed");
    assert!(
        stdout.contains("captures verified: 8"),
        "unexpected capture count"
    );
    assert!(stdout.contains("PASS"));
    let _ = std::fs::remove_dir_all(&base);
}

/// The server path: `request_frame` against the real layer, with the harness presenting
/// continuously. Each PNG is checked against the frame number in its own pixels, and against the
/// `presentIndex` the layer reported. Under validation when `CS2VK_VALIDATION_DIR` is set.
#[test]
#[ignore = "needs a Vulkan ICD and a built layer; see the module docs"]
fn request_frame_end_to_end() {
    let base = scratch("e2e");
    let cap = base.join("capture");
    private_dir(&cap);
    let h = spawn_harness(&base, &cap, &[]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        let mut checked = Vec::new();
        for k in 0..3 {
            let id = format!("e2e{k}");
            let png = request_frame(&cap, &id, Duration::from_secs(20))
                .await
                .map_err(|e| format!("request {id}: {e:#}"))?;
            let meta: serde_json::Value = serde_json::from_slice(
                &std::fs::read(cap.join(format!("{id}.json"))).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            let pi = meta["presentIndex"].as_u64().ok_or("no presentIndex")?;
            let rgb = load_png_rgb(&png).map_err(|e| format!("{e:#}"))?;
            testpattern::verify_rgb(rgb.width, rgb.height, &rgb.pixels, pi)?;
            checked.push(pi);
        }
        Ok::<Vec<u64>, String>(checked)
    });
    // stop the harness through its stop file so it exits and flushes its logs
    std::fs::write(base.join("stop"), b"").unwrap();
    let log = h.log.clone();
    let status = h.finish(Duration::from_secs(60));
    assert!(status.success(), "harness did not exit cleanly");
    let checked = result.expect("end-to-end captures");
    println!("request_frame captures verified at presents {checked:?}");
    assert_eq!(checked.len(), 3);
    check_validation(&log);
    let _ = std::fs::remove_dir_all(&base);
}

/// The deferred path, live: with `CS2MCP_CAPTURE_FENCE_TIMEOUT_MS=0` the readback thread never
/// waits, so each capture's GPU objects go to the device's deferred list and each request is
/// answered with the "did not finish" error. The harness must keep presenting and exit cleanly
/// (deferred objects are freed by polling on later presents and at device destruction).
#[test]
#[ignore = "needs a Vulkan ICD and a built layer; see the module docs"]
fn fence_timeout_defers_and_recovers() {
    let base = scratch("fence");
    let cap = base.join("capture");
    private_dir(&cap);
    let h = spawn_harness(&base, &cap, &[("CS2MCP_CAPTURE_FENCE_TIMEOUT_MS", "0")]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcomes = rt.block_on(async {
        let mut outcomes = Vec::new();
        for k in 0..4 {
            let id = format!("f{k}");
            match request_frame(&cap, &id, Duration::from_secs(20)).await {
                Ok(png) => {
                    let meta: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(cap.join(format!("{id}.json"))).unwrap(),
                    )
                    .unwrap();
                    let pi = meta["presentIndex"].as_u64().unwrap();
                    let rgb = load_png_rgb(&png).unwrap();
                    testpattern::verify_rgb(rgb.width, rgb.height, &rgb.pixels, pi)
                        .expect("a frame that did arrive is pixel-exact");
                    outcomes.push("frame".to_string());
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    assert!(msg.contains("did not finish"), "unexpected failure: {msg}");
                    outcomes.push("deferred".to_string());
                }
            }
        }
        outcomes
    });
    std::fs::write(base.join("stop"), b"").unwrap();
    let status = h.finish(Duration::from_secs(60));
    println!("fence-timeout outcomes: {outcomes:?}");
    // with the wait disabled, no capture can come back as a frame: all take the deferred path
    assert!(outcomes.iter().all(|o| o == "deferred"), "{outcomes:?}");
    assert!(status.success(), "harness did not exit cleanly");
    let log = base.join("harness.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!text.contains("panicked"), "a panic reached the log");
    check_validation(&log);
    let _ = std::fs::remove_dir_all(&base);
}
