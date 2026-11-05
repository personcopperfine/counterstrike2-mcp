//! Judges: assertions over what a finished run left behind (console.log lines, PNG screenshots,
//! SourceTV demos). The library is generic; `move_jump` is the POC judge ported check by check.
//! Line numbers in comments cite `idabins/worknotes/vtest/check_move_jump.py`.
//!
//! Each check is `{name, passed, detail}`. Names of checks the POC does not have carry an
//! "(added)" suffix: they read the same evidence more strictly and do not change the POC checks.
use cs2_mcp_common::capture::frame::{self, ImageStats};
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// check_move_jump.py:63 (`os.path.getsize(files[0]) > 20000`).
pub const SCREENSHOT_MIN_BYTES: u64 = 20_000;
/// check_move_jump.py:71 (`os.path.getsize(dems[0]) > 10000`).
pub const DEMO_MIN_BYTES: u64 = 10_000;
/// (added) a frame is non-blank when this fraction of pixels has a channel above 8
/// (`frame::NONBLACK_THRESHOLD`) and it has more than one colour.
pub const NONBLANK_MIN_FRAC: f64 = 0.01;

pub const MOVE_JUMP_MARKERS: [&str; 4] = [
    "VTEST_BEGIN",
    "VTEST_MAP_LOADED",
    "VTEST_SPAWNED",
    "VTEST_DONE",
];
const MOVE_JUMP_TAGS: [&str; 4] = ["spawn", "moved", "midjump", "landed"];
const MOVE_JUMP_SHOTS: [&str; 3] = ["spawn", "moved", "midjump"];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
    /// Not evaluated (e.g. ffprobe absent on the host). Such a check is neither passed nor failed:
    /// `passed` is false and it does not fail the verdict. Serialized only when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

impl Check {
    pub fn new(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            passed,
            detail: detail.into(),
            skipped: false,
        }
    }

    /// A check that was not evaluated. Never counts as passed.
    pub fn skipped(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            passed: false,
            detail: detail.into(),
            skipped: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub passed: bool,
    pub checks: Vec<Check>,
    /// Artifact file names collected into the run's artifacts dir.
    pub artifacts: Vec<String>,
}

impl Verdict {
    pub fn new(checks: Vec<Check>, artifacts: Vec<String>) -> Self {
        Verdict {
            passed: checks.iter().all(|c| c.passed || c.skipped),
            checks,
            artifacts,
        }
    }
}

/// What a judge reads: the copied console.log and the run's artifacts directory.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub console: String,
    pub dir: PathBuf,
}

impl Evidence {
    /// Read `<dir>/console.log` (empty when absent: "console.log present" then fails).
    pub fn load(dir: &Path) -> Evidence {
        let console = std::fs::read(dir.join("console.log"))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        Evidence {
            console,
            dir: dir.to_path_buf(),
        }
    }
}

pub type JudgeFn = fn(&Evidence) -> Vec<Check>;

/// Judge presets selectable by name (`run_testscript {judge}`).
pub const PRESETS: &[(&str, JudgeFn)] = &[("move_jump", move_jump)];

pub fn known_preset(name: &str) -> bool {
    PRESETS.iter().any(|(n, _)| *n == name)
}

pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _)| *n).collect()
}

/// Checks for a finished run: the named preset (if any), then the demo check when `demo` is set.
pub fn checks_for(preset: Option<&str>, demo: bool, ev: &Evidence) -> Result<Vec<Check>, String> {
    let mut checks = Vec::new();
    if let Some(name) = preset {
        let (_, judge) = PRESETS.iter().find(|(n, _)| *n == name).ok_or_else(|| {
            format!(
                "unknown judge preset {name:?} (known: {})",
                preset_names().join(", ")
            )
        })?;
        checks.extend(judge(ev));
    }
    if demo {
        checks.push(demo_check(&ev.dir, "vtest_", DEMO_MIN_BYTES, true));
    }
    Ok(checks)
}

fn is_marker(line: &str, marker: &str) -> bool {
    line.contains(&format!("CheckPoint -> '{marker}'"))
}

/// One check per marker: `TESTSCRIPT: CheckPoint -> '<name>'` appears in console.log.
pub fn markers_present(lines: &[&str], markers: &[&str]) -> Vec<Check> {
    markers
        .iter()
        .map(|m| {
            let found = lines.iter().any(|l| is_marker(l, m));
            Check::new(
                format!("marker {m}"),
                found,
                if found { "found" } else { "not in console.log" },
            )
        })
        .collect()
}

/// (added) the markers occur in the given order (each one after the previous).
pub fn markers_in_order(lines: &[&str], markers: &[&str]) -> Check {
    let mut at = 0usize;
    let mut seen = Vec::new();
    for m in markers {
        match lines[at..].iter().position(|l| is_marker(l, m)) {
            Some(p) => {
                seen.push(format!("{m}@{}", at + p + 1));
                at += p + 1;
            }
            None => {
                return Check::new(
                    "markers in order (added)",
                    false,
                    format!("{m} not found after line {at}; seen: {}", seen.join(" < ")),
                )
            }
        }
    }
    Check::new("markers in order (added)", true, seen.join(" < "))
}

static POS_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"CheckPoint -> 'VTEST_POS_(\w+)'").expect("regex"));
static SETPOS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"setpos\s+(-?[\d.]+)\s+(-?[\d.]+)\s+(-?[\d.]+)").expect("regex"));

/// Tagged `getpos` samples. The `VTEST_POS_<tag>` checkpoint is followed (command-buffer order) by
/// the `setpos x y z;setang ...` line that getpos prints. check_move_jump.py:36-47.
pub fn position_samples(lines: &[&str]) -> HashMap<String, [f64; 3]> {
    let mut pos = HashMap::new();
    let mut want: Option<String> = None;
    for line in lines {
        if let Some(c) = POS_TAG.captures(line) {
            want = Some(c[1].to_string());
            continue;
        }
        if let (Some(c), Some(tag)) = (SETPOS.captures(line), want.as_ref()) {
            let parsed: Option<Vec<f64>> = (1..=3).map(|i| c[i].parse().ok()).collect();
            if let Some(v) = parsed {
                pos.insert(tag.clone(), [v[0], v[1], v[2]]);
            }
            want = None;
        }
    }
    pos
}

/// Files in `dir` named `<prefix>*<suffix>`, sorted (a glob, like the POC's `glob` + `sorted`).
fn files_with(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(prefix) && name.ends_with(suffix) && p.is_file()
        })
        .collect();
    files.sort();
    files
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Demo check: the first `<prefix>*.dem` is larger than `min_bytes`. The name lists every match
/// like check_move_jump.py:70-71; `added` marks the generic check that the POC does not have.
pub fn demo_check(dir: &Path, prefix: &str, min_bytes: u64, added: bool) -> Check {
    let dems = files_with(dir, prefix, ".dem");
    let ok = dems.first().is_some_and(|d| file_size(d) > min_bytes);
    let listing: Vec<String> = dems
        .iter()
        .map(|d| format!("{} {} B", file_name(d), file_size(d)))
        .collect();
    let what = if listing.is_empty() {
        "missing".to_string()
    } else {
        listing.join(", ")
    };
    let name = if added {
        format!("demo recorded (added): {what}")
    } else {
        format!("demo recorded ({what})")
    };
    Check::new(name, ok, format!("limit > {min_bytes} B"))
}

/// (added) the screenshot decodes and is not a blank frame. Takes the bytes already read for the
/// POC checks, so each PNG is read once and decoded once.
fn nonblank_check(bytes: &[u8], tag: &str) -> Check {
    let name = format!("screenshot {tag} non-blank (added)");
    match frame::decode_png(bytes).map(|img| img.stats()) {
        Ok(ImageStats {
            nonblack_frac,
            unique_colors,
            ..
        }) => Check::new(
            name,
            unique_colors > 1 && nonblack_frac >= NONBLANK_MIN_FRAC,
            format!("nonblackFrac {nonblack_frac:.4}, uniqueColors {unique_colors}"),
        ),
        Err(e) => Check::new(name, false, format!("{e:#}")),
    }
}

/// Port of check_move_jump.py, check for check, in its order. The POC asserts presence and
/// thresholds; the (added) checks follow them.
pub fn move_jump(ev: &Evidence) -> Vec<Check> {
    let lines: Vec<&str> = ev.console.lines().collect();
    let mut c = Vec::new();
    // :30 console.log present (the POC tests the text is non-empty)
    c.push(Check::new(
        "console.log present",
        !ev.console.is_empty(),
        format!("{} lines", lines.len()),
    ));
    // :33-34 the four markers
    c.extend(markers_present(&lines, &MOVE_JUMP_MARKERS));
    // :36-49 getpos samples, one per tag
    let pos = position_samples(&lines);
    for tag in MOVE_JUMP_TAGS {
        match pos.get(tag) {
            Some(p) => c.push(Check::new(
                format!("position sample '{tag}' {p:?}"),
                true,
                "setpos parsed",
            )),
            None => c.push(Check::new(
                format!("position sample '{tag}' "),
                false,
                "no setpos after its checkpoint",
            )),
        }
    }
    // :51-53 moved forward: horizontal distance spawn -> moved > 50
    if let (Some(s), Some(m)) = (pos.get("spawn"), pos.get("moved")) {
        let d = ((s[0] - m[0]).powi(2) + (s[1] - m[1]).powi(2)).sqrt();
        c.push(Check::new(
            format!("moved forward: {d:.1} units horizontally (> 50)"),
            d > 50.0,
            "limit > 50",
        ));
    }
    // :54-56 jump: mid-air z above the moved z by more than 10
    if let (Some(m), Some(j)) = (pos.get("moved"), pos.get("midjump")) {
        let dz = j[2] - m[2];
        c.push(Check::new(
            format!("jump: mid-air z +{dz:.1} units (> 10)"),
            dz > 10.0,
            "limit > 10",
        ));
    }
    // :57-58 landed: z back below the mid-air z by more than 5
    if let (Some(j), Some(l)) = (pos.get("midjump"), pos.get("landed")) {
        c.push(Check::new(
            format!("landed: z back down to {:.1}", l[2]),
            l[2] < j[2] - 5.0,
            format!("limit < midjump z - 5 = {:.1}", j[2] - 5.0),
        ));
    }
    // :60-66 screenshots exist and are larger than 20000 bytes. Each first file is read once; the
    // bytes serve the size check, the comparison below and the (added) decode.
    let mut shots: HashMap<&str, Vec<u8>> = HashMap::new();
    for tag in MOVE_JUMP_SHOTS {
        let first = files_with(&ev.dir, &format!("vtest_{tag}"), ".png")
            .into_iter()
            .next();
        let bytes = first.as_ref().and_then(|f| std::fs::read(f).ok());
        let size = bytes.as_ref().map_or(0, |b| b.len() as u64);
        let ok = first.is_some() && size > SCREENSHOT_MIN_BYTES;
        let shown = first
            .as_deref()
            .map(file_name)
            .unwrap_or_else(|| "missing".into());
        c.push(Check::new(
            format!("screenshot {tag} ({shown})"),
            ok,
            format!("limit > {SCREENSHOT_MIN_BYTES} B"),
        ));
        if let Some(b) = bytes {
            shots.insert(tag, b);
        }
    }
    // :67-68 the screenshots differ after moving (byte comparison = the POC's sha256 comparison)
    if let (Some(a), Some(b)) = (shots.get("spawn"), shots.get("moved")) {
        c.push(Check::new(
            "screenshots differ after moving",
            a != b,
            "byte-identical files fail",
        ));
    }
    // :70-71 demo recorded
    c.push(demo_check(
        &ev.dir,
        "vtest_move_jump",
        DEMO_MIN_BYTES,
        false,
    ));

    // (added) stricter reads of the same evidence
    c.push(markers_in_order(&lines, &MOVE_JUMP_MARKERS));
    for tag in MOVE_JUMP_SHOTS {
        match shots.get(tag) {
            Some(b) => c.push(nonblank_check(b, tag)),
            None => c.push(Check::new(
                format!("screenshot {tag} non-blank (added)"),
                false,
                "no screenshot",
            )),
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_PASS: &str = include_str!("../../fixtures/testscript/move_jump_pass.console.log");
    const FIXTURE_NOJUMP: &str =
        include_str!("../../fixtures/testscript/move_jump_nojump.console.log");

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("judge-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A noisy frame: many colours, many nonblack pixels, a PNG well above 20000 bytes.
    fn noisy_png(seed: u32) -> Vec<u8> {
        let (w, h) = (320u32, 200u32);
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let pixels: Vec<u8> = (0..(w * h * 3))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            })
            .collect();
        frame::RgbImage::new(w, h, pixels)
            .unwrap()
            .to_png()
            .unwrap()
    }

    /// Artifacts of a passing run, built from the fixture console.log.
    fn run_dir(tag: &str, console: &str, demo_bytes: usize, shot_seed: u32) -> PathBuf {
        let dir = temp_dir(tag);
        std::fs::write(dir.join("console.log"), console).unwrap();
        for (i, tag) in MOVE_JUMP_SHOTS.iter().enumerate() {
            std::fs::write(
                dir.join(format!("vtest_{tag}_0000.png")),
                noisy_png(shot_seed + i as u32),
            )
            .unwrap();
        }
        std::fs::write(dir.join("vtest_move_jump.dem"), vec![7u8; demo_bytes]).unwrap();
        dir
    }

    fn failed(checks: &[Check]) -> Vec<&str> {
        checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| c.name.as_str())
            .collect()
    }

    #[test]
    fn poc_fixture_passes_every_check_and_has_the_poc_count() {
        let dir = run_dir("pass", FIXTURE_PASS, 278_867, 1);
        let ev = Evidence::load(&dir);
        let checks = move_jump(&ev);
        assert!(failed(&checks).is_empty(), "{:#?}", failed(&checks));
        // 17 POC checks (check_move_jump.py: 1 + 4 + 4 + 3 + 1 + 1 + 1 + 1 + 1) and 4 added ones
        let added = checks.iter().filter(|c| c.name.contains("(added)")).count();
        assert_eq!(checks.len() - added, 17, "{checks:#?}");
        assert_eq!(added, 4, "markers in order + 3 non-blank");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn position_parsing_follows_the_checkpoint_then_setpos_rule() {
        let lines: Vec<&str> = FIXTURE_PASS.lines().collect();
        let pos = position_samples(&lines);
        assert_eq!(pos["spawn"], [160.122742, 2369.676270, -56.481201]);
        assert_eq!(pos["midjump"][2], 13.163074);
        assert_eq!(pos.len(), 4);
    }

    #[test]
    fn a_jump_that_never_leaves_the_ground_fails_only_the_jump_checks() {
        let dir = run_dir("nojump", FIXTURE_NOJUMP, 278_867, 2);
        let checks = move_jump(&Evidence::load(&dir));
        let failing = failed(&checks);
        assert_eq!(failing.len(), 2, "{failing:?}");
        assert!(
            failing
                .iter()
                .any(|n| n.starts_with("jump: mid-air z +0.0")),
            "{failing:?}"
        );
        assert!(
            failing.iter().any(|n| n.starts_with("landed: z back down")),
            "{failing:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_markers_and_empty_evidence_fail_without_panicking() {
        let dir = temp_dir("empty");
        let checks = move_jump(&Evidence::load(&dir));
        assert!(
            !checks
                .iter()
                .find(|c| c.name == "console.log present")
                .unwrap()
                .passed
        );
        assert!(checks
            .iter()
            .filter(|c| c.name.starts_with("marker "))
            .all(|c| !c.passed));
        assert!(checks
            .iter()
            .any(|c| c.name == "screenshot spawn (missing)" && !c.passed));
        assert!(checks
            .iter()
            .any(|c| c.name.starts_with("demo recorded (missing)")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markers_must_appear_in_order() {
        let lines = [
            "x CheckPoint -> 'VTEST_MAP_LOADED'",
            "x CheckPoint -> 'VTEST_BEGIN'",
        ];
        let c = markers_in_order(&lines, &["VTEST_BEGIN", "VTEST_MAP_LOADED"]);
        assert!(!c.passed, "{c:?}");
        let ok = markers_in_order(&lines, &["VTEST_MAP_LOADED", "VTEST_BEGIN"]);
        assert!(ok.passed, "{ok:?}");
    }

    #[test]
    fn a_flat_black_screenshot_is_not_nonblank() {
        let black = frame::RgbImage::new(64, 64, vec![0u8; 64 * 64 * 3])
            .unwrap()
            .to_png()
            .unwrap();
        let c = nonblank_check(&black, "x");
        assert!(!c.passed, "{c:?}");
        assert!(nonblank_check(&noisy_png(9), "x").passed);
        assert!(!nonblank_check(b"not a png", "x").passed);
    }

    #[test]
    fn checks_for_rejects_unknown_presets_and_adds_the_demo_check() {
        let dir = run_dir("cf", FIXTURE_PASS, 20_000, 3);
        let ev = Evidence::load(&dir);
        assert!(checks_for(Some("nope"), false, &ev).is_err());
        let with_demo = checks_for(Some("move_jump"), true, &ev).unwrap();
        assert!(with_demo
            .last()
            .unwrap()
            .name
            .starts_with("demo recorded (added)"));
        assert!(with_demo.last().unwrap().passed);
        assert!(checks_for(None, false, &ev).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verdict_passes_only_when_every_check_passes() {
        let ok = Check::new("a", true, "");
        let bad = Check::new("b", false, "");
        assert!(Verdict::new(vec![ok.clone()], vec![]).passed);
        assert!(!Verdict::new(vec![ok, bad], vec![]).passed);
        assert!(Verdict::new(vec![], vec![]).passed);
    }
}
