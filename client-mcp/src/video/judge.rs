//! Judge for a demo-to-video run (`demo_to_video`). Evidence: the run's console.log (checkpoints,
//! recorder start, the "Finishing movie" count) and the output in the game dir (`csgo/`), which
//! must be newer than the run.
//!
//! Checks that need no tool: the checkpoints, the recorder lines, the file size (>= 10000 B,
//! check_movie_demo.py:25), and the EBML magic of a webm. Checks that need ffprobe: one video
//! stream with an AV1/VPx codec, the frame count (packets by default, decoded frames with
//! `countFrames`) within 2% of the recorder, and `r_frame_rate` == `<fps>/1`. Without ffprobe those
//! are reported as skipped, never passed. The contact sheet needs ffmpeg the same way.
//!
//! The "Finishing movie" line format is movie-scratch.md (`Finishing movie: %s.* [%llu frames @
//! %f == %f seconds]%s`), and check_movie_demo.py:24 reads it the same way.
use super::MovieFormat;
use crate::testscript::judge::{markers_present, Check};
use regex::Regex;
use serde_json::Value;
use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Decoded frames (or packets) may differ from the recorder's count by this fraction.
pub const FRAME_TOLERANCE: f64 = 0.02;
/// check_movie_demo.py:25 (`> 10_000`); the brief asks for `>= 10000`.
pub const MIN_VIDEO_BYTES: u64 = 10_000;
/// The EBML header that starts every webm (matroska) file.
pub const EBML_MAGIC: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
/// The probe deadline is at least this, or 100 ms per recorded frame, whichever is longer.
pub const PROBE_MIN_DEADLINE: Duration = Duration::from_secs(60);
pub const PROBE_PER_FRAME_MS: u64 = 100;
pub const CONTACT_DEADLINE: Duration = Duration::from_secs(60);
pub const CONTACT_SHEET: &str = "contact.png";
/// How much of stderr a failed tool call keeps in its check detail.
pub const STDERR_CAP: usize = 300;
const POLL: Duration = Duration::from_millis(20);

/// What the run was asked to produce and where the engine should have written it.
#[derive(Debug, Clone)]
pub struct MovieExpect {
    pub name: String,
    pub format: MovieFormat,
    pub framerate: u32,
    /// `<app>/game/csgo`: the engine's output directory (`no_unique_dir`)
    pub game_dir: PathBuf,
    /// output older than this belongs to an earlier run
    pub since: SystemTime,
    /// ffprobe on the host; `None` = absent, so the probe checks are skipped
    pub ffprobe: Option<PathBuf>,
    /// ffmpeg on the host; `None` = absent, so the contact sheet is skipped
    pub ffmpeg: Option<PathBuf>,
    /// `-count_frames` (decodes every frame, slow) instead of `-count_packets`
    pub count_frames: bool,
    /// overrides [`probe_deadline`] (tests); `None` = the default
    pub probe_timeout: Option<Duration>,
}

impl MovieExpect {
    /// The video file for webm (`<name>.webm`).
    pub fn video_file(&self) -> PathBuf {
        self.game_dir
            .join(format!("{}.{}", self.name, self.format.as_str()))
    }
}

/// The checks and the artifact names the judge added (the contact sheet).
#[derive(Debug, Clone, Default)]
pub struct Judgment {
    pub checks: Vec<Check>,
    pub artifacts: Vec<String>,
}

pub const STREAM_CHECK: &str = "video stream count == 1 (added)";
pub const FPS_CHECK_PREFIX: &str = "r_frame_rate";
pub const CONTACT_CHECK: &str = "contact sheet (added)";

/// The frame-count check name for the counting mode in use.
pub fn frames_check_name(count_frames: bool) -> &'static str {
    if count_frames {
        "decoded frames match recorder count (added)"
    } else {
        "video packets match recorder count (added)"
    }
}

/// `frames` from the recorder's "Finishing movie: <name>.* [N frames" line (the first one).
pub fn recorder_frames(console: &str, name: &str) -> Option<u64> {
    let re = Regex::new(&format!(
        r"Finishing movie: {}\.\* \[(\d+) frames",
        regex::escape(name)
    ))
    .ok()?;
    re.captures(console)?.get(1)?.as_str().parse().ok()
}

/// Within 2% of the recorder's count. A zero or missing count never matches.
pub fn frame_count_ok(got: u64, want: u64) -> bool {
    want > 0 && (got as f64 - want as f64).abs() <= want as f64 * FRAME_TOLERANCE
}

/// The deadline for one ffprobe call on a movie with `frames` recorded frames.
pub fn probe_deadline(frames: u64) -> Duration {
    PROBE_MIN_DEADLINE.max(Duration::from_millis(
        frames.saturating_mul(PROBE_PER_FRAME_MS),
    ))
}

/// True when `path` is a file, non-empty, modified at or after `since`: returns its size.
fn fresh_size(path: &Path, since: SystemTime) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let fresh = meta.is_file() && meta.modified().is_ok_and(|m| m >= since);
    (fresh && meta.len() > 0).then_some(meta.len())
}

/// Frame files `<name>NNNNNNNN.<ext>` in the game dir that this run wrote.
fn frame_files(e: &MovieExpect) -> Vec<PathBuf> {
    let re = Regex::new(&format!(
        r"^{}\d{{8}}\.{}$",
        regex::escape(&e.name),
        e.format.as_str()
    ))
    .expect("escaped name and fixed extension make a valid regex");
    let Ok(entries) = std::fs::read_dir(&e.game_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|ent| re.is_match(&ent.file_name().to_string_lossy()))
        .map(|ent| ent.path())
        .filter(|p| fresh_size(p, e.since).is_some())
        .collect()
}

/// The checks for a finished demo-to-video run. `console` is the run's console.log text, `dir`
/// the run's artifacts dir (the contact sheet goes there).
pub fn movie_checks(console: &str, e: &MovieExpect, dir: &Path) -> Judgment {
    let lines: Vec<&str> = console.lines().collect();
    let mut c = markers_present(&lines, &["DemoPlaybackStarted", "DemoPlaybackFinished"]);
    let started = Regex::new(&format!(
        r"Started recording movie: {}\.\*",
        regex::escape(&e.name)
    ))
    .expect("escaped name");
    c.push(Check::new(
        format!("movie recorder started ({})", e.name),
        started.is_match(console),
        "Started recording movie line",
    ));
    let want = recorder_frames(console, &e.name);
    c.push(Check::new(
        format!("movie recorder finished ({})", e.name),
        want.is_some_and(|n| n > 0),
        match want {
            Some(n) => format!("Finishing movie reports {n} frames"),
            None => "no Finishing movie line".to_string(),
        },
    ));
    let want = want.unwrap_or(0);
    let mut judgment = Judgment {
        checks: c,
        artifacts: Vec::new(),
    };
    match e.format {
        MovieFormat::Webm => video_checks(e, want, dir, &mut judgment),
        _ => frame_checks(e, want, &mut judgment.checks),
    }
    judgment
}

fn video_checks(e: &MovieExpect, want: u64, dir: &Path, j: &mut Judgment) {
    let file = e.video_file();
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let size = fresh_size(&file, e.since);
    j.checks.push(Check::new(
        format!("{name} written >= {MIN_VIDEO_BYTES} B (added)"),
        size.is_some_and(|s| s >= MIN_VIDEO_BYTES),
        match size {
            Some(s) => format!("{s} B"),
            None => format!("missing, empty, or older than the run: {}", file.display()),
        },
    ));
    let magic = size.and_then(|_| read_head(&file));
    j.checks.push(Check::new(
        format!("{name} has the EBML magic (added)"),
        magic.as_deref() == Some(&EBML_MAGIC[..]),
        match &magic {
            Some(h) => format!("first bytes {}", hex(h)),
            None => "no output file to read".into(),
        },
    ));

    match &e.ffprobe {
        None => j.checks.extend(probe_skipped(e)),
        Some(_) if size.is_none() => {
            j.checks.extend(probe_failed(e, "no output file to probe"));
        }
        Some(bin) => {
            let args = probe_args(e.count_frames, &file);
            let deadline = e.probe_timeout.unwrap_or_else(|| probe_deadline(want));
            match run_timed(bin, &args, deadline) {
                Ok(json) => j
                    .checks
                    .extend(probe_checks(&json, want, e.framerate, e.count_frames)),
                Err(RunErr::Timeout(d)) => {
                    j.checks.push(Check::new(
                        "ffprobe timed out (added)",
                        false,
                        format!("no answer after {} s; killed", d.as_secs()),
                    ));
                    j.checks.extend(probe_failed(e, "ffprobe timed out"));
                }
                Err(RunErr::Failed(msg)) => j.checks.extend(probe_failed(e, &msg)),
            }
        }
    }

    match (&e.ffmpeg, size) {
        (None, _) => j.checks.push(Check::skipped(
            CONTACT_CHECK,
            "skipped: ffmpeg not found on the host",
        )),
        (Some(_), None) => {
            j.checks
                .push(Check::new(CONTACT_CHECK, false, "no output file to tile"))
        }
        (Some(bin), Some(_)) => {
            let out = dir.join(CONTACT_SHEET);
            let _ = std::fs::remove_file(&out);
            let result = run_timed(bin, &contact_args(&file, &out), CONTACT_DEADLINE);
            let made = out.metadata().is_ok_and(|m| m.is_file() && m.len() > 0);
            match (result, made) {
                (Ok(_), true) => {
                    j.checks.push(Check::new(
                        CONTACT_CHECK,
                        true,
                        format!("{CONTACT_SHEET}: every 40th frame, tiled 3x3"),
                    ));
                    j.artifacts.push(CONTACT_SHEET.to_string());
                }
                (Ok(_), false) => j.checks.push(Check::new(
                    CONTACT_CHECK,
                    false,
                    "ffmpeg ran but wrote no contact sheet",
                )),
                (Err(err), _) => j
                    .checks
                    .push(Check::new(CONTACT_CHECK, false, err.to_string())),
            }
        }
    }
}

fn read_head(path: &Path) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).ok()?;
    Some(buf.to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The probe checks, not evaluated (no ffprobe on the host).
fn probe_skipped(e: &MovieExpect) -> Vec<Check> {
    let detail = "skipped: ffprobe not found on the host";
    vec![
        Check::skipped(STREAM_CHECK, detail),
        Check::skipped(frames_check_name(e.count_frames), detail),
        Check::skipped(fps_check_name(e.framerate), detail),
    ]
}

/// The probe checks, failed with `why` (the probe could not run or produced nothing usable).
fn probe_failed(e: &MovieExpect, why: &str) -> Vec<Check> {
    vec![
        Check::new(STREAM_CHECK, false, why),
        Check::new(frames_check_name(e.count_frames), false, why),
        Check::new(fps_check_name(e.framerate), false, why),
    ]
}

fn fps_check_name(fps: u32) -> String {
    format!("{FPS_CHECK_PREFIX} == {fps}/1 (added)")
}

/// Image sequences: the frame files are the output. Counted, not probed.
fn frame_checks(e: &MovieExpect, want: u64, c: &mut Vec<Check>) {
    let got = frame_files(e).len() as u64;
    c.push(Check::new(
        format!(
            "frame files written ({}NNNNNNNN.{}) (added)",
            e.name,
            e.format.as_str()
        ),
        got > 0,
        format!("{got} files"),
    ));
    c.push(Check::new(
        "frame files match recorder count (added)",
        frame_count_ok(got, want),
        format!("{got} files, recorder {want} frames, tolerance 2%"),
    ));
}

/// Arguments for ffprobe on one file (check_movie_demo.py:33 with packets by default).
pub fn probe_args(count_frames: bool, file: &Path) -> Vec<OsString> {
    let mut entries =
        "stream=codec_type,codec_name,width,height,r_frame_rate,nb_read_packets".to_string();
    let mut args: Vec<OsString> = vec!["-v".into(), "error".into()];
    if count_frames {
        args.push("-count_frames".into());
        entries.push_str(",nb_read_frames");
    } else {
        args.push("-count_packets".into());
    }
    args.extend([
        "-show_entries".into(),
        entries.into(),
        "-of".into(),
        "json".into(),
    ]);
    args.push(file.as_os_str().to_owned());
    args
}

/// Arguments for the contact sheet: every 40th frame, scaled, tiled 3x3, one PNG.
pub fn contact_args(file: &Path, out: &Path) -> Vec<OsString> {
    vec![
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        file.as_os_str().to_owned(),
        "-vf".into(),
        r"select=not(mod(n\,40)),scale=480:-1,tile=3x3".into(),
        "-frames:v".into(),
        "1".into(),
        out.as_os_str().to_owned(),
    ]
}

/// Pure: the three probe checks from an `ffprobe -of json` document. The frame count is
/// `nb_read_frames` when `count_frames`, else `nb_read_packets` (one packet per frame for webm).
pub fn probe_checks(json: &str, want: u64, fps: u32, count_frames: bool) -> Vec<Check> {
    let v: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            return probe_failed_raw(
                &format!("ffprobe output is not JSON: {e}"),
                fps,
                count_frames,
            )
        }
    };
    let streams = v
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let videos: Vec<&Value> = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .collect();
    let first = videos.first().copied();
    // check_movie_demo.py:37: AV1 or VPx (raw bytes can probe as "video" too)
    let codec = first
        .and_then(|s| s.get("codec_name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let stream_check = Check::new(
        STREAM_CHECK,
        videos.len() == 1 && matches!(codec, "av1" | "vp8" | "vp9"),
        format!(
            "{} streams, {} video, codec {}",
            streams.len(),
            videos.len(),
            if codec.is_empty() { "-" } else { codec }
        ),
    );
    let key = if count_frames {
        "nb_read_frames"
    } else {
        "nb_read_packets"
    };
    let got = first.and_then(|s| s.get(key)).and_then(|f| match f {
        Value::String(s) => s.parse::<u64>().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    });
    let frame_check = match got {
        Some(got) => Check::new(
            frames_check_name(count_frames),
            frame_count_ok(got, want),
            format!("{key} {got}, recorder {want}, tolerance 2%"),
        ),
        None => Check::new(
            frames_check_name(count_frames),
            false,
            format!("ffprobe reported no {key} for the video stream"),
        ),
    };
    let rate = first
        .and_then(|s| s.get("r_frame_rate"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    let fps_check = Check::new(
        fps_check_name(fps),
        rate == format!("{fps}/1"),
        format!("r_frame_rate {rate}"),
    );
    vec![stream_check, frame_check, fps_check]
}

fn probe_failed_raw(msg: &str, fps: u32, count_frames: bool) -> Vec<Check> {
    vec![
        Check::new(STREAM_CHECK, false, msg),
        Check::new(frames_check_name(count_frames), false, msg),
        Check::new(fps_check_name(fps), false, msg),
    ]
}

/// Why a tool call did not produce output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunErr {
    /// killed after the deadline
    Timeout(Duration),
    /// did not start, exited non-zero (stderr truncated), or could not be waited for
    Failed(String),
}

impl fmt::Display for RunErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunErr::Timeout(d) => write!(f, "timed out after {} s", d.as_secs()),
            RunErr::Failed(m) => f.write_str(m),
        }
    }
}

/// Run `bin args` and return its stdout. The child is polled (never waited on blindly) and killed
/// at `deadline`. Stderr is truncated to [`STDERR_CAP`] characters in the error.
pub fn run_timed(bin: &Path, args: &[OsString], deadline: Duration) -> Result<String, RunErr> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunErr::Failed(format!("{} did not start: {e}", bin.display())))?;
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_t = thread::spawn(move || read_all(out_pipe));
    let err_t = thread::spawn(move || read_all(err_pipe));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunErr::Failed(format!(
                    "waiting for {}: {e}",
                    bin.display()
                )));
            }
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunErr::Timeout(deadline));
        }
        thread::sleep(POLL.min(deadline.saturating_sub(started.elapsed())));
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    let code = status
        .code()
        .map_or_else(|| "by signal".to_string(), |c| format!("code {c}"));
    Err(RunErr::Failed(format!(
        "{} exited {code}: {}",
        bin.display(),
        truncate(&String::from_utf8_lossy(&stderr), STDERR_CAP)
    )))
}

fn read_all(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        let _ = p.read_to_end(&mut buf);
    }
    buf
}

fn truncate(s: &str, cap: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= cap {
        t.to_string()
    } else {
        format!("{}...", t.chars().take(cap).collect::<String>())
    }
}

/// Copy the run's webm into the artifacts dir when this run wrote it. Returns its name.
pub fn collect_video(e: &MovieExpect, out: &Path) -> std::io::Result<Option<String>> {
    if e.format != MovieFormat::Webm {
        return Ok(None);
    }
    let file = e.video_file();
    if fresh_size(&file, e.since).is_none() {
        return Ok(None);
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::fs::create_dir_all(out)?;
    std::fs::copy(&file, out.join(&name))?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{PoisonError, RwLock, RwLockReadGuard};

    /// POC excerpt (idabins/worknotes/vtest/artifacts/movie_demo-20261010-093325/console.log).
    const POC_PASS: &str = include_str!("../../fixtures/video/movie_demo_pass.console.log");

    const PROBE_OK: &str = r#"{"streams":[
        {"codec_type":"video","codec_name":"av1","width":1024,"height":720,"r_frame_rate":"30/1","nb_read_packets":"337"},
        {"codec_type":"audio","codec_name":"vorbis"}]}"#;
    const PROBE_NO_VIDEO: &str = r#"{"streams":[{"codec_type":"audio","codec_name":"vorbis"}]}"#;

    fn expect(dir: &Path, format: MovieFormat) -> MovieExpect {
        MovieExpect {
            name: "vtest_movie".into(),
            format,
            framerate: 30,
            game_dir: dir.to_path_buf(),
            since: SystemTime::now() - Duration::from_secs(3600),
            ffprobe: None,
            ffmpeg: None,
            count_frames: false,
            probe_timeout: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d =
            std::env::temp_dir().join(format!("video-judge-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A webm-looking file: EBML magic then padding to `len` bytes.
    fn webm_bytes(len: usize) -> Vec<u8> {
        let mut v = EBML_MAGIC.to_vec();
        v.resize(len, 0);
        v
    }

    fn find<'a>(checks: &'a [Check], prefix: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name.starts_with(prefix))
            .unwrap_or_else(|| panic!("no check {prefix:?} in {checks:?}"))
    }

    /// Script files are written under the write lock and run under the read lock (`spawning`). A
    /// child forked by another test while a script is still open for writing keeps that fd until
    /// its exec, and exec of the script then fails with ETXTBSY ("Text file busy").
    static SCRIPT_FILES: RwLock<()> = RwLock::new(());

    fn spawning() -> RwLockReadGuard<'static, ()> {
        SCRIPT_FILES.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// An executable shell script at `dir/<name>` with the given body.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let _w = SCRIPT_FILES.write().unwrap_or_else(PoisonError::into_inner);
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn judge(console: &str, e: &MovieExpect, dir: &Path) -> Vec<Check> {
        let _s = spawning();
        movie_checks(console, e, dir).checks
    }

    #[test]
    fn recorder_count_comes_from_the_finishing_line() {
        assert_eq!(recorder_frames(POC_PASS, "vtest_movie"), Some(337));
        assert_eq!(recorder_frames(POC_PASS, "other_movie"), None);
        // the name is a literal: `.` in a name must not match anything else
        assert_eq!(recorder_frames(POC_PASS, "vtest_movi"), None);
        assert_eq!(
            recorder_frames("Finishing movie: x.* [0 frames @ 30", "x"),
            Some(0)
        );
    }

    #[test]
    fn two_percent_tolerance_bounds() {
        assert!(frame_count_ok(337, 337));
        assert!(frame_count_ok(331, 337)); // 6/337 = 1.78%
        assert!(!frame_count_ok(330, 337)); // 7/337 = 2.08%
        assert!(frame_count_ok(102, 100));
        assert!(!frame_count_ok(103, 100));
        assert!(!frame_count_ok(0, 0), "a zero count never matches");
    }

    #[test]
    fn probe_deadline_is_a_minute_or_a_tenth_second_per_frame() {
        assert_eq!(probe_deadline(10), Duration::from_secs(60));
        assert_eq!(probe_deadline(3_000), Duration::from_secs(300));
        assert_eq!(probe_deadline(u64::MAX), Duration::from_millis(u64::MAX));
    }

    #[test]
    fn probe_args_count_packets_by_default_and_frames_on_opt_in() {
        let f = Path::new("/x/m.webm");
        let packets: Vec<String> = probe_args(false, f)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(packets.contains(&"-count_packets".to_string()));
        assert!(!packets.contains(&"-count_frames".to_string()));
        assert!(packets.iter().any(|a| a.contains("nb_read_packets")));
        assert!(!packets.iter().any(|a| a.contains("nb_read_frames")));
        let frames: Vec<String> = probe_args(true, f)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(frames.contains(&"-count_frames".to_string()));
        assert!(frames.iter().any(|a| a.contains("nb_read_frames")));
        assert_eq!(frames.last().unwrap(), "/x/m.webm");
    }

    #[test]
    fn probe_passes_on_one_av1_stream_with_matching_packets_and_rate() {
        let c = probe_checks(PROBE_OK, 337, 30, false);
        assert_eq!(c.len(), 3);
        assert!(c.iter().all(|c| c.passed && !c.skipped), "{c:?}");
        assert!(find(&c, "video packets").passed);
    }

    #[test]
    fn probe_fails_on_no_video_raw_codec_frame_drift_and_wrong_rate() {
        let c = probe_checks(PROBE_NO_VIDEO, 337, 30, false);
        assert!(!find(&c, "video stream count").passed);
        let raw = r#"{"streams":[{"codec_type":"video","codec_name":"rawvideo","nb_read_packets":"337","r_frame_rate":"30/1"}]}"#;
        assert!(!find(&probe_checks(raw, 337, 30, false), "video stream count").passed);
        let c = probe_checks(PROBE_OK, 330, 30, false);
        assert!(!find(&c, "video packets").passed, "{c:?}");
        let c = probe_checks(PROBE_OK, 337, 60, false);
        assert!(!find(&c, "r_frame_rate == 60/1").passed, "{c:?}");
        let bad = probe_checks("not json", 1, 30, false);
        assert!(bad.iter().all(|c| !c.passed && !c.skipped));
    }

    #[test]
    fn decoded_frames_are_read_when_counting_frames() {
        let with_frames = r#"{"streams":[{"codec_type":"video","codec_name":"av1","r_frame_rate":"30/1","nb_read_packets":"300","nb_read_frames":"337"}]}"#;
        let c = probe_checks(with_frames, 337, 30, true);
        assert!(find(&c, "decoded frames").passed, "{c:?}");
        // packets alone do not satisfy the frame mode
        let c = probe_checks(with_frames, 300, 30, true);
        assert!(!find(&c, "decoded frames").passed, "{c:?}");
    }

    #[test]
    fn missing_tools_skip_the_probe_and_contact_checks_and_never_pass_them() {
        let d = temp_dir("skip");
        std::fs::write(d.join("vtest_movie.webm"), webm_bytes(12_000)).unwrap();
        let checks = judge(POC_PASS, &expect(&d, MovieFormat::Webm), &d);
        for prefix in [
            "video stream count",
            "video packets",
            "r_frame_rate",
            "contact sheet",
        ] {
            let c = find(&checks, prefix);
            assert!(c.skipped && !c.passed, "{c:?}");
        }
        let verdict = crate::testscript::judge::Verdict::new(checks.clone(), vec![]);
        assert!(verdict.passed, "{checks:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn size_and_magic_are_checked_without_any_tool() {
        let d = temp_dir("magic");
        std::fs::write(d.join("vtest_movie.webm"), webm_bytes(9_999)).unwrap();
        let checks = judge(POC_PASS, &expect(&d, MovieFormat::Webm), &d);
        assert!(!find(&checks, "vtest_movie.webm written").passed);
        assert!(find(&checks, "vtest_movie.webm has the EBML magic").passed);

        std::fs::write(d.join("vtest_movie.webm"), vec![0x52u8; 12_000]).unwrap();
        let checks = judge(POC_PASS, &expect(&d, MovieFormat::Webm), &d);
        assert!(find(&checks, "vtest_movie.webm written").passed);
        assert!(!find(&checks, "vtest_movie.webm has the EBML magic").passed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_or_stale_output_fails_the_file_checks() {
        let d = temp_dir("missing");
        let checks = judge(POC_PASS, &expect(&d, MovieFormat::Webm), &d);
        assert!(!find(&checks, "vtest_movie.webm written").passed);
        std::fs::write(d.join("vtest_movie.webm"), b"").unwrap();
        let checks = judge(POC_PASS, &expect(&d, MovieFormat::Webm), &d);
        assert!(!find(&checks, "vtest_movie.webm written").passed);
        std::fs::write(d.join("vtest_movie.webm"), webm_bytes(12_000)).unwrap();
        let mut e = expect(&d, MovieFormat::Webm);
        e.since = SystemTime::now() + Duration::from_secs(60);
        let checks = judge(POC_PASS, &e, &d);
        assert!(!find(&checks, "vtest_movie.webm written").passed);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn happy_path_with_a_fake_ffprobe_and_ffmpeg_passes_every_check() {
        let d = temp_dir("happy");
        let bins = temp_dir("happy-bins");
        std::fs::write(d.join("vtest_movie.webm"), webm_bytes(12_000)).unwrap();
        std::fs::write(bins.join("probe.json"), PROBE_OK).unwrap();
        let ffprobe = script(
            &bins,
            "ffprobe",
            &format!("cat {}", bins.join("probe.json").display()),
        );
        // the fake ffmpeg writes its last argument (the output path)
        let ffmpeg = script(
            &bins,
            "ffmpeg",
            "for a; do out=$a; done; printf 'PNG' > \"$out\"",
        );
        let mut e = expect(&d, MovieFormat::Webm);
        e.ffprobe = Some(ffprobe);
        e.ffmpeg = Some(ffmpeg);
        let j = {
            let _s = spawning();
            movie_checks(POC_PASS, &e, &d)
        };
        assert!(j.checks.iter().all(|c| c.passed), "{:?}", j.checks);
        assert!(!j.checks.iter().any(|c| c.skipped));
        assert_eq!(j.artifacts, vec![CONTACT_SHEET.to_string()]);
        assert!(d.join(CONTACT_SHEET).is_file());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&bins);
    }

    #[test]
    fn ffprobe_present_with_the_file_missing_fails_the_probe_checks() {
        let d = temp_dir("nofile");
        let bins = temp_dir("nofile-bins");
        std::fs::write(bins.join("probe.json"), PROBE_OK).unwrap();
        let ffprobe = script(
            &bins,
            "ffprobe",
            &format!("cat {}", bins.join("probe.json").display()),
        );
        let mut e = expect(&d, MovieFormat::Webm);
        e.ffprobe = Some(ffprobe);
        let checks = judge(POC_PASS, &e, &d);
        for prefix in ["video stream count", "video packets", "r_frame_rate"] {
            let c = find(&checks, prefix);
            assert!(!c.passed && !c.skipped, "{c:?}");
        }
        assert!(!find(&checks, "vtest_movie.webm written").passed);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&bins);
    }

    #[test]
    fn a_probe_that_hangs_is_killed_and_recorded_as_a_timeout() {
        let bins = temp_dir("hang");
        // exec: the kill reaches the sleep itself, not a shell child that would outlive it
        let slow = script(&bins, "ffprobe", "exec sleep 30");
        let t = Instant::now();
        let err = {
            let _s = spawning();
            run_timed(&slow, &[], Duration::from_millis(300)).unwrap_err()
        };
        assert!(matches!(err, RunErr::Timeout(_)), "{err:?}");
        assert!(t.elapsed() < Duration::from_secs(10), "killed promptly");
        let _ = std::fs::remove_dir_all(&bins);
    }

    #[test]
    fn a_failing_probe_reports_truncated_stderr() {
        let bins = temp_dir("fail");
        let noisy = script(
            &bins,
            "ffprobe",
            "head -c 2000 /dev/zero | tr '\\0' e >&2; exit 3",
        );
        let err = {
            let _s = spawning();
            run_timed(&noisy, &[], Duration::from_secs(10)).unwrap_err()
        };
        match err {
            RunErr::Failed(m) => {
                assert!(m.contains("exited code 3"), "{m}");
                assert!(
                    m.len() < STDERR_CAP + 200,
                    "stderr truncated: {} bytes",
                    m.len()
                );
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&bins);
    }

    #[test]
    fn a_hanging_probe_fails_the_run_with_ffprobe_timed_out() {
        let d = temp_dir("timeout-check");
        let bins = temp_dir("timeout-check-bins");
        std::fs::write(d.join("vtest_movie.webm"), webm_bytes(12_000)).unwrap();
        // exec: the kill reaches the sleep itself, not a shell child that would outlive it
        let slow = script(&bins, "ffprobe", "exec sleep 30");
        let mut e = expect(&d, MovieFormat::Webm);
        e.ffprobe = Some(slow);
        e.probe_timeout = Some(Duration::from_millis(300));
        let checks = judge(POC_PASS, &e, &d);
        let t = find(&checks, "ffprobe timed out");
        assert!(!t.passed && !t.skipped, "{t:?}");
        for prefix in ["video stream count", "video packets", "r_frame_rate"] {
            let c = find(&checks, prefix);
            assert!(!c.passed && c.detail.contains("ffprobe timed out"), "{c:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&bins);
    }

    #[test]
    fn png_judge_counts_frame_files_against_the_recorder() {
        let d = temp_dir("frames");
        for i in 0..3 {
            std::fs::write(d.join(format!("vtest_movie{i:08}.png")), b"p").unwrap();
        }
        std::fs::write(d.join("vtest_movie_other00000000.png"), b"p").unwrap();
        std::fs::write(d.join("vtest_movie.png"), b"p").unwrap();
        let log = "Finishing movie: vtest_movie.* [3 frames @ 30.000000 == 0.1 seconds]\n\
                   Started recording movie: vtest_movie.*: framerate 30.000000\n";
        let checks = judge(log, &expect(&d, MovieFormat::Png), &d);
        assert!(find(&checks, "frame files written").passed, "{checks:?}");
        assert!(find(&checks, "frame files match").passed, "{checks:?}");
        assert!(checks.iter().all(|c| !c.name.contains("stream count")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn collect_copies_only_a_fresh_webm() {
        let d = temp_dir("collect");
        let out = d.join("out");
        let e = expect(&d, MovieFormat::Webm);
        assert_eq!(collect_video(&e, &out).unwrap(), None);
        std::fs::write(d.join("vtest_movie.webm"), b"abc").unwrap();
        assert_eq!(
            collect_video(&e, &out).unwrap().as_deref(),
            Some("vtest_movie.webm")
        );
        assert_eq!(std::fs::read(out.join("vtest_movie.webm")).unwrap(), b"abc");
        let png = expect(&d, MovieFormat::Png);
        assert_eq!(collect_video(&png, &out).unwrap(), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
