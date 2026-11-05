//! Live test: the s2sdk-probe payload inside a real CS2 dedicated server (WP6b).
//!
//! Builds the payload (`crates/s2sdk/docker/build-probe.sh`), then launches a LinuxDedicated session
//! the same way client-mcp's `server_up` does — through cs2-mcp-common's `DockerRuntime` with
//! `DockerOptions::s2sdk_probe_dir` set, which mounts the payload at `/cs2mcp/s2sdk` and makes
//! `lib-preamble.sh` prepend it to `LD_PRELOAD` — and waits (bounded) for BOTH:
//!
//! * a live-server marker in the session's `server.log` (`SERVER_LIVE_MARKERS`), and
//! * the probe's `[s2sdk-probe] RESULT` line (its stderr is teed into the same file),
//!
//! then asserts the RESULT is `ok` at stage `sdk`, parses `/work/s2sdk-probe.json` from the
//! artifacts dir and checks the report: `ok`, `stage`, every mandatory check
//! (`schemasystem`, `schema_system`, `schema_scope`, `schema_class`, `cvar`) `ok` with a
//! `data` object, and the non-mandatory `events` check present (it may fail: the engine's
//! `GameEventsManager002` exposure is the known unknown — the JSON is printed either way).
//! Exactly one container is started and it is always torn down (the drop guard SIGKILLs and
//! removes it even when an assert fails).
//!
//! Run with:
//!
//! ```text
//! cargo test -p s2sdk-probe --test live -- --ignored --nocapture
//! ```
//!
//! Needs podman, the `cs2-home` volume and the `ghcr.io/personcopperfine/cs2-docker` image. Knobs:
//! `CS2_MCP_PODMAN`, `CS2_MCP_PROJECT_ROOT` (same meanings as client-mcp's flags),
//! `CS2SDK_LIVE_MAP` (default `de_dust2`), `CS2SDK_LIVE_TIMEOUT_SECS` (default 360 — the first
//! boot can be slow).
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cs2_mcp_common::runtime::docker::{server_game_args, stop_argv, DockerOptions, DockerRuntime};
use cs2_mcp_common::runtime::readiness::SERVER_LIVE_MARKERS;
use cs2_mcp_common::runtime::{LaunchSpec, SessionKind};

/// The checks the RESULT line's `ok` is computed from (the probe's `MANDATORY`): every one of
/// them has to be `ok` in the report.
const MANDATORY_CHECKS: [&str; 5] = [
    "schemasystem",
    "schema_system",
    "schema_scope",
    "schema_class",
    "cvar",
];

/// The workspace root this test was compiled from (a checkout/worktree of cs2-mcp): its
/// `client-mcp/docker/` scripts are mounted into the container, and its `target/` holds the
/// built payload.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("workspace root exists")
}

/// `CARGO_TARGET_DIR` if set, else `<workspace root>/target` (never /tmp: RAM tmpfs).
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("target"))
}

/// Same resolution as client-mcp's config: `CS2_MCP_PROJECT_ROOT`, else the checkout root
/// (`cs2-mcp-common`'s `DEFAULT_PROJECT_ROOT` — the workspace this test was built in).
fn project_root() -> PathBuf {
    std::env::var_os("CS2_MCP_PROJECT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .to_path_buf()
        })
}

fn live_map() -> String {
    std::env::var("CS2SDK_LIVE_MAP").unwrap_or_else(|_| "de_dust2".into())
}

fn live_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("CS2SDK_LIVE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(360),
    )
}

/// Builds the payload and returns the directory holding `libs2sdk_probe.so` (the path that
/// goes into `DockerOptions::s2sdk_probe_dir`; the same one build-probe.sh installs into).
fn build_probe() -> PathBuf {
    let dest = target_dir().join("s2sdk-probe");
    let script = repo_root().join("crates/s2sdk/docker/build-probe.sh");
    let out = Command::new(&script)
        .arg(&dest)
        .env("CARGO_TARGET_DIR", target_dir())
        .output()
        .expect("run crates/s2sdk/docker/build-probe.sh");
    assert!(
        out.status.success(),
        "build-probe.sh failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let so = dest.join("libs2sdk_probe.so");
    assert!(
        so.is_file(),
        "{} was not installed (build-probe.sh said it did)",
        so.display()
    );
    dest
}

/// SIGKILLs and removes the session container on ANY exit path (assert failure, panic). The
/// success path stops the session properly through the runtime and disarms the guard.
///
/// WP7a observed `podman stop -t 0` on a booted LinuxDedicated container once failing to land
/// ("given PID did not die within timeout" from crun, while a direct host `kill -9` of the
/// same PIDs worked instantly), so the guard verifies and, as a last resort, SIGKILLs the
/// container's processes directly (they are visible in the host PID namespace; their
/// `/proc/<pid>/cgroup` line names the container's scope).
struct Guard {
    podman: PathBuf,
    name: String,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = Command::new(&self.podman)
            .args(stop_argv(&self.name))
            .status();
        if !self.container_exists() {
            return;
        }
        // podman's signal did not land (or the container is still going down): kill the
        // container's processes from the host, then remove
        self.kill_container_pids();
        std::thread::sleep(Duration::from_millis(500));
        let _ = Command::new(&self.podman)
            .args(["rm", "-f", &self.name])
            .status();
    }
}

impl Guard {
    fn container_exists(&self) -> bool {
        Command::new(&self.podman)
            .args(["ps", "-a", "--filter", &format!("name={}", self.name)])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&self.name))
            .unwrap_or(false)
    }

    /// SIGKILLs every host-visible process whose `/proc/<pid>/cgroup` line names this
    /// container (that line carries the container's ID, which is unique). This is the last
    /// resort for a podman stop signal that did not land.
    fn kill_container_pids(&self) {
        let inspected = Command::new(&self.podman)
            .args(["inspect", "--format", "{{.ID}}", &self.name])
            .output()
            .ok()
            .filter(|o| o.status.success());
        let Some(out) = inspected else {
            return;
        };
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return;
        }
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return;
        };
        for entry in procs.flatten() {
            let Ok(pid) = entry.file_name().into_string() else {
                continue;
            };
            if !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Ok(line) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) {
                if line.contains(&id) {
                    let _ = Command::new("kill").args(["-9", &pid]).status();
                }
            }
        }
    }
}

/// Everything the failure messages want: where the artifacts landed.
fn artifact_paths(artifacts: &Path) -> String {
    format!(
        "artifacts dir: {}\n  server.log: {}\n  probe json: {}",
        artifacts.display(),
        artifacts.join("server.log").display(),
        artifacts.join("s2sdk-probe.json").display()
    )
}

/// One check of the parsed report by name (the report carries every check on every path).
fn check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|c| c["name"] == name))
        .unwrap_or_else(|| panic!("check {name:?} missing from the report: {report}"))
}

/// The SDK layer in a live Linux dedicated server, end to end.
///
/// The launch runs the real `bridge/install-rs.sh --platform linux ensure` on the `cs2-home`
/// volume first (the runtime does that for every LinuxDedicated launch), so this needs the
/// checkout's Linux-capable `bridge/install-rs.sh` as well as podman, the volume and the image.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs podman, the cs2-home volume and the ghcr.io/personcopperfine/cs2-docker image"]
async fn probe_reports_the_sdk_layer_in_a_live_dedicated_server() {
    let probe_dir = build_probe();
    let podman = std::env::var_os("CS2_MCP_PODMAN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("podman"));
    let runtime = DockerRuntime::new(DockerOptions {
        // like client-mcp: one budget unit per running cs2* container; the dedicated server
        // takes no GPU, so this stays inside the machine's budget even next to other sessions
        s2sdk_probe_dir: Some(probe_dir),
        linux_client_script: repo_root().join("client-mcp/docker/linux-client.sh"),
        ..DockerOptions::new(project_root(), podman.clone(), 4)
    });

    // the server_up launch shape (client-mcp/src/tools/multiplayer.rs): a LAN anonymous
    // dedicated server; the probe rides on the container's LD_PRELOAD, not on the game args
    let spec = LaunchSpec {
        kind: SessionKind::LinuxDedicated,
        testscript: None,
        map: None,
        args: server_game_args(&live_map(), None, &[]),
        headless: true,
        gpus: false,
        container_name: None,
        capture: false,
        bridge: true,
    };
    let launched = runtime
        .launch(&spec)
        .await
        .expect("LinuxDedicated launch with the probe dir");
    let mut guard = Guard {
        podman: podman.clone(),
        name: launched.container.clone(),
        armed: true,
    };
    let artifacts = launched.artifacts_dir.clone();
    let log_path = artifacts.join("server.log");
    let json_path = artifacts.join("s2sdk-probe.json");
    println!(
        "container {} (reused: {})\n{}",
        launched.container,
        launched.reused,
        artifact_paths(&artifacts)
    );

    // one bounded wait for both conditions, paced, no fixed sleeps-as-waits; the log is read
    // incrementally (offset + full-chunk search, so a marker split across reads is still found)
    let started = Instant::now();
    let deadline = started + live_timeout();
    let mut seen = String::new(); // everything appended to server.log so far
    let mut offset = 0u64;
    let mut live_marker = false;
    let mut result_line: Option<String> = None;
    while Instant::now() < deadline {
        if let Ok(mut f) = std::fs::File::open(&log_path) {
            use std::io::{Read, Seek, SeekFrom};
            if f.seek(SeekFrom::Start(offset)).is_ok() {
                let mut chunk = Vec::new();
                if f.read_to_end(&mut chunk).is_ok() {
                    offset += chunk.len() as u64;
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
        }
        live_marker |= SERVER_LIVE_MARKERS.iter().any(|m| seen.contains(m));
        if result_line.is_none() {
            result_line = seen
                .lines()
                .find(|l| l.contains("[s2sdk-probe] RESULT "))
                .map(str::to_string);
        }
        if live_marker && result_line.is_some() {
            break;
        }
        // fail fast: the script prints SERVER_EXIT when the server process is gone
        if seen.contains("SERVER_EXIT=") && result_line.is_none() {
            panic!(
                "the server exited before the probe reported:\n  seen: {}\n  {}",
                seen.lines().rev().take(20).collect::<Vec<_>>().join("\n  "),
                artifact_paths(&artifacts)
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let result_line = result_line
        .unwrap_or_else(|| {
            panic!(
                "no [s2sdk-probe] RESULT line within {} s (live marker seen: {live_marker}):\n  tail: {}\n  {}",
                started.elapsed().as_secs(),
                seen.lines().rev().take(20).collect::<Vec<_>>().join("\n  "),
                artifact_paths(&artifacts)
            )
        });
    println!("probe RESULT line: {result_line}");
    assert!(
        result_line.contains("[s2sdk-probe] RESULT ok stage=sdk"),
        "probe did not report ok/sdk ({result_line}):\n  {}",
        artifact_paths(&artifacts)
    );
    assert!(
        seen.contains("[s2sdk-probe] loaded pid="),
        "the constructor's early line is missing from server.log:\n  {}",
        artifact_paths(&artifacts)
    );

    // the JSON report in the same artifacts dir
    let json_text = std::fs::read_to_string(&json_path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\n  {}",
            json_path.display(),
            artifact_paths(&artifacts)
        )
    });
    println!("probe JSON: {json_text}");
    let report: serde_json::Value = serde_json::from_str(&json_text)
        .unwrap_or_else(|e| panic!("probe JSON does not parse: {e}\n  {json_text}"));
    assert_eq!(
        report["probe"], "s2sdk-probe",
        "unexpected report: {json_text}"
    );
    assert_eq!(report["stage"], "sdk", "unexpected stage: {json_text}");
    assert_eq!(report["ok"], true, "unexpected ok: {json_text}");
    let pid = report["pid"].as_u64().unwrap_or(0);
    assert!(pid > 0, "pid missing in the report: {json_text}");
    let exe = report["exe"].as_str().unwrap_or("");
    assert_eq!(
        Path::new(exe).file_name().and_then(|n| n.to_str()),
        Some("cs2"),
        "the probe ran in the wrong process ({exe}) or reported the wrong exe:\n  {json_text}"
    );

    // every mandatory check passed, and every check carries its evidence object
    for name in MANDATORY_CHECKS {
        let c = check(&report, name);
        assert_eq!(
            c["ok"], true,
            "mandatory check {name} failed: {c}\n  {json_text}"
        );
        assert_eq!(
            c["mandatory"], true,
            "mandatory check {name} is not flagged mandatory: {c}\n  {json_text}"
        );
        assert!(
            c["data"].is_object(),
            "mandatory check {name} has no data object: {c}\n  {json_text}"
        );
        println!("check {name}: {}", c["detail"]);
    }
    // the non-mandatory event-manager check: reported either way (its data object is the
    // finding when the engine does not export GameEventsManager002)
    let events = check(&report, "events");
    assert_eq!(
        events["mandatory"], false,
        "the events check must be non-mandatory: {events}\n  {json_text}"
    );
    println!(
        "check events (non-mandatory, ok={}): {} | data: {}",
        events["ok"], events["detail"], events["data"]
    );
    // the live facts this test exists to pin: the server scope name, the cvar library and the
    // base chain (printed above in the JSON; asserted only for presence here)
    println!(
        "scope: {} | cvar from: {} | CBaseEntity size {} fields {}",
        check(&report, "schema_scope")["data"]["scope_name"],
        check(&report, "cvar")["data"]["serve_module"],
        check(&report, "schema_class")["data"]["size"],
        check(&report, "schema_class")["data"]["field_count"]
    );

    // clean teardown; the guard takes over if this fails. Verify like the Drop path does
    // (see the Guard docs: podman's stop signal failed to land on a booted server once).
    let stopped = runtime
        .stop_named(&launched.container, Duration::from_secs(30))
        .await
        .map(|o| format!("{o:?}"))
        .unwrap_or_else(|e| format!("error: {e}"));
    println!("teardown: {stopped}");
    for _ in 0..3 {
        if !guard.container_exists() {
            break;
        }
        guard.kill_container_pids();
        std::thread::sleep(Duration::from_millis(1000));
        let _ = Command::new(&podman)
            .args(["rm", "-f", &launched.container])
            .status();
    }
    assert!(
        !guard.container_exists(),
        "the session container survived teardown: {}",
        launched.container
    );
    guard.armed = false;
}
