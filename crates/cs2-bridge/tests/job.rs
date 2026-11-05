//! `job.start` / `job.status` / `job.cancel` over a fake process (`ops::job`): the parser, the
//! validator, the log scanner, the table with its watcher threads, the reply shapes and the
//! lane wiring. Nothing here starts a real compiler; the live test covers that.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cs2_bridge::ops::job::{
    self, build_argv, parse_flags, parse_ok_line, parse_start, validate_input, Host, JobTable,
    Layout, LogScanner, Process, SpawnError, SpawnSpec, StartRequest, MAX_FINISHED,
};
use serde_json::Value;

const HOST_EXE: &str = "/t/app/game/bin/win64/cs2.exe";
const INPUT: &str = "csgo_addons/my_addon/maps/a.vmap";

struct FakeProc {
    pid: u32,
    exit: Mutex<Option<i64>>,
    terminated: AtomicBool,
    log: String,
}

impl FakeProc {
    fn write(&self, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(&self.log).expect("log");
        file.write_all(bytes).expect("write");
    }
    fn exit(&self, code: i64) {
        *self.exit.lock().unwrap() = Some(code);
    }
    fn terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }
}

struct Handle(Arc<FakeProc>);

impl Process for Handle {
    fn pid(&self) -> u32 {
        self.0.pid
    }
    fn exit_code(&mut self) -> Option<i64> {
        *self.0.exit.lock().unwrap()
    }
    fn terminate(&mut self) {
        self.0.terminated.store(true, Ordering::SeqCst);
    }
}

struct FakeHost {
    layout: Layout,
    files: Mutex<HashSet<String>>,
    procs: Mutex<Vec<Arc<FakeProc>>>,
    specs: Mutex<Vec<SpawnSpec>>,
    fail_spawn: AtomicBool,
    prereq_spawn: AtomicBool,
    /// When set, `spawn` announces itself on `.0` and waits for `.1` before it returns.
    hold: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    pids: AtomicU32,
}

impl Host for FakeHost {
    fn layout(&self) -> Result<Layout, String> {
        Ok(self.layout.clone())
    }
    fn is_file(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains(path)
    }
    fn spawn(&self, spec: &SpawnSpec) -> Result<Box<dyn Process>, SpawnError> {
        if let Some((entered, release)) = self.hold.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        if self.prereq_spawn.load(Ordering::SeqCst) {
            return Err(SpawnError::Prerequisite("job: AssignProcessToJobObject failed: denied".to_string()));
        }
        if self.fail_spawn.load(Ordering::SeqCst) {
            return Err(SpawnError::Failed("boom".to_string()));
        }
        std::fs::create_dir_all(PathBuf::from(&spec.log_path).parent().unwrap()).unwrap();
        std::fs::write(&spec.log_path, b"").unwrap();
        let proc = Arc::new(FakeProc {
            pid: 1000 + self.pids.fetch_add(1, Ordering::SeqCst),
            exit: Mutex::new(None),
            terminated: AtomicBool::new(false),
            log: spec.log_path.clone(),
        });
        self.procs.lock().unwrap().push(Arc::clone(&proc));
        self.specs.lock().unwrap().push(spec.clone());
        Ok(Box::new(Handle(proc)))
    }
}

struct Fixture {
    host: Arc<FakeHost>,
    table: JobTable,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("job-{name}"));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut layout = Layout::from_host_exe(HOST_EXE, '/').expect("layout");
        layout.log_dir = scratch.display().to_string();
        let files: HashSet<String> =
            [layout.exe.clone(), format!("{}/{INPUT}", layout.content_root)].into_iter().collect();
        let host = Arc::new(FakeHost {
            layout,
            files: Mutex::new(files),
            procs: Mutex::new(Vec::new()),
            specs: Mutex::new(Vec::new()),
            fail_spawn: AtomicBool::new(false),
            prereq_spawn: AtomicBool::new(false),
            hold: Mutex::new(None),
            pids: AtomicU32::new(0),
        });
        let table = JobTable::new(host.clone(), Duration::from_millis(5))
            .with_nonce("t")
            .with_fatal_grace(Duration::from_millis(60));
        Fixture { host, table }
    }

    fn start(&self) -> Value {
        self.start_with(r#"{"kind":"resourcecompiler","input":"csgo_addons/my_addon/maps/a.vmap"}"#)
    }

    fn start_with(&self, params: &str) -> Value {
        parse(&self.table.start(Some(params)).expect("job.start"))
    }

    fn proc(&self, n: usize) -> Arc<FakeProc> {
        Arc::clone(&self.host.procs.lock().unwrap()[n])
    }

    fn status(&self, id: &str) -> Value {
        parse(&self.table.status(Some(&format!(r#"{{"jobId":"{id}"}}"#))).expect("job.status"))
    }

    fn wait_state(&self, id: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = self.status(id);
            if status["state"] == state {
                return status;
            }
            assert!(Instant::now() < deadline, "job {id} never reached {state}: {status}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).expect("json")
}

fn err(reply: job::Reply) -> (i64, String, Value) {
    let (code, message, data) = reply.expect_err("an error reply");
    (code, message.to_string(), parse(&data))
}

#[test]
fn layout_is_found_from_the_host_image() {
    let layout = Layout::from_host_exe(r"Z:\app\game\bin\win64\cs2.exe", '\\').unwrap();
    assert_eq!(layout.exe, r"Z:\app\game\bin\win64\resourcecompiler.exe");
    assert_eq!(layout.cwd, r"Z:\app\game\bin\win64");
    assert_eq!(layout.game_dir, r"Z:\app\game");
    assert_eq!(layout.content_root, r"Z:\app\content");
    assert_eq!(layout.log_dir, r"Z:\app\cs2mcp_jobs");
    assert_eq!(layout.log_dir_rel, "cs2mcp_jobs");
    for bad in [r"Z:\app\game\bin\win32\cs2.exe", r"C:\cs2.exe", "cs2.exe", r"Z:\game\bin\win64\cs2.exe"] {
        assert!(Layout::from_host_exe(bad, '\\').is_err(), "{bad}");
    }
}

/// A native install has no `Z:\app`: the logs go under `%LOCALAPPDATA%\cs2mcpbridge\jobs`,
/// never into the user's Steam tree; the container keeps `<app>\cs2mcp_jobs`.
#[test]
fn native_job_logs_stay_out_of_the_install() {
    let exe = r"C:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe";
    let local = Some(r"C:\Users\Jörg\AppData\Local\");
    let native = Layout::from_host_exe(exe, '\\').unwrap().with_native_log_dir(false, local);
    assert_eq!(native.log_dir, r"C:\Users\Jörg\AppData\Local\cs2mcpbridge\jobs");
    assert_eq!(native.log_dir_rel, "");
    assert_eq!(native.content_root, r"C:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive\content");
    let container = Layout::from_host_exe(r"Z:\app\game\bin\win64\cs2.exe", '\\').unwrap();
    assert_eq!(container.clone().with_native_log_dir(true, local), container);
    // no LOCALAPPDATA at all: the old directory (better a log in the tree than none)
    assert_eq!(container.clone().with_native_log_dir(false, None), container);
}

#[test]
fn input_validation_refuses_every_trick() {
    assert!(validate_input(INPUT).is_ok());
    for ok in [
        "csgo_addons/a/particles/x.vpcf",
        "csgo_addons/a/models/x.VMDL",
        "csgo_addons/a/soundevents/x.vsndevts",
        "csgo_addons/a/materials/x.vmat",
        "csgo_addons/a/maps/scripts/main.js",
    ] {
        assert!(validate_input(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "maps/a.vmap",
        "csgo_addons/a.vmap",
        "csgo_addons/a/../b/x.vmap",
        "csgo_addons/a/./x.vmap",
        "csgo_addons//a/x.vmap",
        "/csgo_addons/a/x.vmap",
        "csgo_addons/a/x.vmap/",
        r"csgo_addons\a\x.vmap",
        "csgo_addons/a/C:/x.vmap",
        "C:/csgo_addons/a/x.vmap",
        "//server/share/csgo_addons/a/x.vmap",
        "csgo_addons/a/x.exe",
        "csgo_addons/a/x",
        "csgo_addons/a/x.vmap_c",
        "csgo_addons/a/x.vjs_c",
        "csgo_addons/a/x.ts",
        "csgo_addons/a/x y.vmap",
        "csgo_addons/a/x\".vmap",
        "csgo_addons/a/x\n.vmap",
        "csgo_addons/a/x%2e%2e.vmap",
    ] {
        assert!(validate_input(bad).is_err(), "{bad:?} must be refused");
    }
    assert!(validate_input(&format!("csgo_addons/a/{}.vmap", "x".repeat(300))).is_err());
}

#[test]
fn flags_are_an_allowlist_with_ranges() {
    let t = |tokens: &[&str]| parse_flags(&tokens.iter().map(ToString::to_string).collect::<Vec<_>>());
    assert!(t(&["-nav", "-fshallow", "-threads", "8", "-maxtextureres", "256", "-quiet", "-unbufferedio", "-noassert", "-nop4", "-f"]).is_ok());
    for bad in [
        &["-i"][..],
        &["-game"],
        &["-exec", "calc"],
        &["-threads"],
        &["-threads", "0"],
        &["-threads", "65"],
        &["-threads", "-1"],
        &["-threads", "8 -x"],
        &["-threads", "0x10"],
        &["-maxtextureres", "16385"],
        &["-nav", "-nav"],
        &["-threads 8"],
        &[""],
    ] {
        assert!(t(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn map_stage_flags_are_allowed() {
    let t = |tokens: &[&str]| parse_flags(&tokens.iter().map(ToString::to_string).collect::<Vec<_>>());
    assert!(t(&["-world", "-phys", "-vis", "-bakelighting", "-nav"]).is_ok());
    assert!(t(&["-threads", "8", "-world", "-phys", "-bakelighting", "-nav"]).is_ok());
    assert!(t(&["-world", "-world"]).is_err());
    assert!(t(&["-gridnav"]).is_err());
}

#[test]
fn params_are_parsed_strictly() {
    let ok = parse_start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}","game":true,"flags":["-nav"],"timeoutMs":5000}}"#))).unwrap();
    assert_eq!(ok.timeout, Duration::from_secs(5));
    assert!(ok.game);
    assert_eq!(ok.flags, ["-nav"]);
    let default = parse_start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))).unwrap();
    assert_eq!(default.timeout, Duration::from_millis(1_500_000));
    for bad in [
        None,
        Some("[]"),
        Some("{"),
        Some(r#"{"input":"csgo_addons/a/x.vmap"}"#),
        Some(r#"{"kind":"vrad","input":"csgo_addons/a/x.vmap"}"#),
        Some(r#"{"kind":"resourcecompiler"}"#),
        Some(r#"{"kind":"resourcecompiler","input":5}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","game":"yes"}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","flags":"-nav"}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","flags":[1]}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","timeoutMs":999}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","timeoutMs":3600001}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","timeoutMs":1.5}"#),
        Some(r#"{"kind":"resourcecompiler","input":"x","timeoutMs":-1}"#),
    ] {
        assert!(parse_start(bad).is_err(), "{bad:?}");
    }
    assert_eq!(default.client_ref, None);
    let tagged = parse_start(Some(&format!(
        r#"{{"kind":"resourcecompiler","input":"{INPUT}","clientRef":"hm-c1.a_B-9"}}"#
    )))
    .unwrap();
    assert_eq!(tagged.client_ref.as_deref(), Some("hm-c1.a_B-9"));
    let too_long = format!("\"{}\"", "x".repeat(65));
    for bad_ref in ["\"\"", "\"a b\"", "7", too_long.as_str()] {
        let text = format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}","clientRef":{bad_ref}}}"#);
        assert!(parse_start(Some(&text)).unwrap_err().contains("clientRef"), "{text}");
    }
}

#[test]
fn argv_shape_for_game_true_and_false() {
    let layout = Layout::from_host_exe(r"Z:\app\game\bin\win64\cs2.exe", '\\').unwrap();
    let request = |game, flags: &[&str]| StartRequest {
        input: INPUT.to_string(),
        game,
        flags: flags.iter().map(ToString::to_string).collect(),
        timeout: Duration::from_secs(5),
        client_ref: None,
    };
    assert_eq!(
        build_argv(&layout, &request(false, &["-threads", "8", "-nav"])),
        [
            r"Z:\app\game\bin\win64\resourcecompiler.exe",
            "-threads",
            "8",
            "-nav",
            "-i",
            r"Z:\app\content\csgo_addons\my_addon\maps\a.vmap"
        ]
    );
    assert_eq!(
        build_argv(&layout, &request(true, &["-nop4"])),
        [
            r"Z:\app\game\bin\win64\resourcecompiler.exe",
            "-nop4",
            "-game",
            r"Z:\app\game\csgo",
            "-i",
            r"Z:\app\content\csgo_addons\my_addon\maps\a.vmap"
        ]
    );
}

#[test]
fn the_ok_line_and_the_scanner() {
    let s = parse_ok_line(" OK: 19 compiled, 0 failed, 1 skipped, 2m:19s").unwrap();
    assert_eq!((s.compiled, s.failed, s.skipped), (19, 0, 1));
    assert_eq!(s.seconds, Some(139.0));
    let bare = parse_ok_line("OK: 1 compiled, 2 failed").unwrap();
    assert_eq!((bare.compiled, bare.failed, bare.skipped, bare.seconds), (1, 2, 0, None));
    assert!(parse_ok_line("NOT OK: 1 compiled, 0 failed").is_none());
    assert!(parse_ok_line("OK: x compiled, 0 failed").is_none());

    let mut scanner = LogScanner::default();
    // Split mid-line, CRLF, a non-UTF-8 byte, a bare [FAIL] after its error line.
    scanner.feed(b"compiling \xff\xfe thing\r\nsome error here\r\n[FA");
    assert!(scanner.fail_lines.is_empty());
    scanner.feed(b"IL]\r\n [FAIL] explicit\r\n OK: 3 compiled, 1 failed, 0 skipped, 5s");
    assert!(scanner.ok.is_none(), "the OK line has no newline yet");
    scanner.finish();
    assert_eq!(scanner.fail_lines, ["[FAIL] some error here", "[FAIL] explicit"]);
    assert_eq!(scanner.ok.as_ref().unwrap().failed, 1);
    assert!(scanner.tail.iter().any(|l| l.contains('\u{fffd}')));
}

#[test]
fn start_reports_the_wire_shape_and_ok_finishes_ok() {
    let f = Fixture::new("ok");
    let started = f.start_with(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}","flags":["-nav"],"game":true}}"#));
    assert_eq!(started["jobId"], "j1-t");
    assert_eq!(started["state"], "running");
    assert_eq!(started["pid"], 1000);
    assert_eq!(started["command"][0], "/t/app/game/bin/win64/resourcecompiler.exe");
    assert_eq!(started["command"][1], "-nav");
    assert_eq!(started["command"][2], "-game");
    assert_eq!(started["command"][3], "/t/app/game/csgo");
    assert_eq!(started["command"][4], "-i");
    assert_eq!(started["command"][5], "/t/app/content/csgo_addons/my_addon/maps/a.vmap");
    assert!(started["startedAtMs"].as_u64().unwrap() > 1_000_000_000_000);
    let rel = started["log"]["relative"].as_str().unwrap();
    assert!(rel.starts_with("cs2mcp_jobs/j1-t") && rel.ends_with(".log"), "{rel}");
    assert!(started["log"]["windows"].as_str().unwrap().ends_with(rel.strip_prefix("cs2mcp_jobs/").unwrap()));
    assert_eq!(f.host.specs.lock().unwrap()[0].cwd, "/t/app/game/bin/win64");

    let p = f.proc(0);
    p.write(b"working\r\n OK: 2 compiled, 0 failed, 1 skipped, 1m:2s\r\n");
    let done = f.wait_state("j1-t", "ok");
    assert!(p.terminated(), "the OK line ends the lingering tree");
    assert_eq!(done["summary"]["compiled"], 2);
    assert_eq!(done["summary"]["skipped"], 1);
    assert_eq!(done["summary"]["seconds"], 62.0);
    assert_eq!(done["failLines"], serde_json::json!([]));
    assert!(done["logTail"].as_str().unwrap().contains("working"));
    assert!(done["elapsedMs"].is_u64());
    assert_eq!(done["kind"], "resourcecompiler");
    assert_eq!(done["log"], started["log"]);
    assert!(done.get("exitCode").is_none());
}

#[test]
fn failed_counts_and_fail_lines() {
    let f = Fixture::new("failed");
    f.start();
    let p = f.proc(0);
    p.write(b"bad material\n[FAIL]\n OK: 1 compiled, 2 failed, 0 skipped\n");
    let done = f.wait_state("j1-t", "failed");
    assert_eq!(done["summary"]["failed"], 2);
    assert_eq!(done["failLines"][0], "[FAIL] bad material");
    assert!(p.terminated());
}

#[test]
fn exit_without_the_ok_line_is_failed_with_the_exit_code() {
    let f = Fixture::new("exit");
    f.start();
    let p = f.proc(0);
    p.write(b"FATAL ERROR: no\n");
    p.exit(3);
    let done = f.wait_state("j1-t", "failed");
    assert_eq!(done["exitCode"], 3);
    assert!(done.get("summary").is_none());
    assert!(done["logTail"].as_str().unwrap().contains("FATAL ERROR"));
    assert!(p.terminated(), "the tree is cleaned up at exit too");
}

#[test]
fn a_log_written_just_before_exit_is_still_read() {
    let f = Fixture::new("late");
    f.start();
    let p = f.proc(0);
    p.write(b"\xff\xfe junk\n OK: 1 compiled, 0 failed, 0 skipped");
    p.exit(0);
    let done = f.wait_state("j1-t", "ok");
    assert_eq!(done["exitCode"], 0);
}

#[test]
fn the_deadline_times_the_job_out() {
    let f = Fixture::new("timeout");
    let layout = f.host.layout.clone();
    let request = StartRequest {
        input: INPUT.to_string(),
        game: false,
        flags: vec![],
        timeout: Duration::from_millis(60),
        client_ref: None,
    };
    f.table.start_request(&layout, &request).expect("start");
    let done = f.wait_state("j1-t", "timeout");
    assert!(f.proc(0).terminated());
    assert!(done["elapsedMs"].as_u64().unwrap() >= 60);
}

#[test]
fn cancel_running_finished_and_unknown() {
    let f = Fixture::new("cancel");
    f.start();
    let cancelled = parse(&f.table.cancel(Some(r#"{"jobId":"j1-t"}"#)).unwrap());
    assert_eq!(cancelled, serde_json::json!({"cancelled": true, "state": "cancelled"}));
    assert!(f.proc(0).terminated());
    assert_eq!(f.status("j1-t")["state"], "cancelled");
    // A late OK line cannot overwrite the cancel.
    f.proc(0).write(b" OK: 1 compiled, 0 failed, 0 skipped\n");
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(f.status("j1-t")["state"], "cancelled");
    let again = parse(&f.table.cancel(Some(r#"{"jobId":"j1-t"}"#)).unwrap());
    assert_eq!(again, serde_json::json!({"cancelled": false, "state": "cancelled"}));

    f.start();
    f.proc(1).write(b" OK: 1 compiled, 0 failed, 0 skipped\n");
    f.wait_state("j2-t", "ok");
    let finished = parse(&f.table.cancel(Some(r#"{"jobId":"j2-t"}"#)).unwrap());
    assert_eq!(finished, serde_json::json!({"cancelled": false, "state": "ok"}));

    let (code, message, data) = err(f.table.cancel(Some(r#"{"jobId":"j99-t"}"#)));
    assert_eq!((code, message.as_str()), (-32602, "InvalidParams"));
    assert_eq!(data["reason"], "unknown jobId");
    assert_eq!(err(f.table.cancel(None)).0, -32602);
    assert_eq!(err(f.table.status(Some(r#"{"jobId":"nope"}"#))).0, -32602);
}

#[test]
fn a_second_start_is_refused_while_one_runs() {
    let f = Fixture::new("second");
    f.start();
    let (code, message, data) = err(f.table.start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))));
    assert_eq!((code, message.as_str()), (-32017, "PrerequisiteFailed"));
    assert_eq!(data["blockedOn"], "job: j1-t running");
    f.proc(0).exit(0);
    f.wait_state("j1-t", "failed");
    let second = f.start();
    assert_eq!(second["jobId"], "j2-t");
}

#[test]
fn finished_jobs_are_evicted_oldest_first() {
    let f = Fixture::new("evict");
    let mut logs = Vec::new();
    for n in 0..(MAX_FINISHED + 4) {
        let id = format!("j{}-t", n + 1);
        logs.push(f.start()["log"]["windows"].as_str().unwrap().to_string());
        f.proc(n).write(b" OK: 1 compiled, 0 failed, 0 skipped\n");
        f.wait_state(&id, "ok");
    }
    // 20 jobs finished: the table keeps the newest MAX_FINISHED.
    let list = parse(&f.table.status(None).unwrap());
    let ids: Vec<&str> = list["jobs"].as_array().unwrap().iter().map(|j| j["jobId"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), MAX_FINISHED, "{ids:?}");
    assert_eq!(ids.last().copied(), Some("j20-t"));
    assert!(!ids.contains(&"j1-t") && !ids.contains(&"j4-t"));
    assert_eq!(ids.first().copied(), Some("j5-t"));
    // Every entry (list and single) echoes the request's input, so a caller whose job.start
    // reply was lost can find its job.
    assert!(list["jobs"].as_array().unwrap().iter().all(|j| j["input"] == "csgo_addons/my_addon/maps/a.vmap"));
    assert_eq!(f.status("j20-t")["input"], "csgo_addons/my_addon/maps/a.vmap");
    assert!(f.status("j20-t")["clientRef"].is_null());
    assert_eq!(err(f.table.status(Some(r#"{"jobId":"j1-t"}"#))).0, -32602);
    assert_eq!(f.table.summaries().len(), ids.len());
    // The evicted jobs' log files are gone, the kept ones stay.
    for (n, log) in logs.iter().enumerate() {
        assert_eq!(std::path::Path::new(log).exists(), n >= 4, "log of j{}", n + 1);
    }
    // A start's clientRef is echoed by the single and the list form.
    f.start_with(r#"{"kind":"resourcecompiler","input":"csgo_addons/my_addon/maps/a.vmap","clientRef":"hm-7"}"#);
    let id = format!("j{}-t", MAX_FINISHED + 5);
    assert_eq!(f.status(&id)["clientRef"], "hm-7");
    let list = parse(&f.table.status(None).unwrap());
    assert_eq!(list["jobs"].as_array().unwrap().last().unwrap()["clientRef"], "hm-7");
}

#[test]
fn start_refusals_each_have_their_code() {
    let f = Fixture::new("refusals");
    let start = |params: &str| err(f.table.start(Some(params)));
    // not under the content root / not a file
    let (code, _, data) = start(r#"{"kind":"resourcecompiler","input":"csgo_addons/my_addon/maps/missing.vmap"}"#);
    assert_eq!(code, -32602);
    assert!(data["reason"].as_str().unwrap().contains("does not exist"));
    assert_eq!(start(r#"{"kind":"resourcecompiler","input":"csgo_addons/../x.vmap"}"#).0, -32602);
    assert_eq!(start(r#"{"kind":"resourcecompiler","input":"csgo_addons/a/x.vmap","flags":["-exec"]}"#).0, -32602);
    assert!(f.host.specs.lock().unwrap().is_empty(), "nothing was spawned");

    // the compiler is missing
    f.host.files.lock().unwrap().remove("/t/app/game/bin/win64/resourcecompiler.exe");
    let (code, _, data) = start(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#));
    assert_eq!(code, -32017);
    assert!(data["blockedOn"].as_str().unwrap().starts_with("job: resourcecompiler.exe not found at"));
    f.host.files.lock().unwrap().insert("/t/app/game/bin/win64/resourcecompiler.exe".to_string());

    // spawn fails
    f.host.fail_spawn.store(true, Ordering::SeqCst);
    let (code, message, data) = start(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#));
    assert_eq!((code, message.as_str()), (-32603, "InternalError"));
    assert!(data["reason"].as_str().unwrap().contains("boom"));
    assert!(f.table.summaries().is_empty(), "a failed spawn leaves no job");

    // a host that cannot contain the tree: -32017, no job (fail closed)
    f.host.fail_spawn.store(false, Ordering::SeqCst);
    f.host.prereq_spawn.store(true, Ordering::SeqCst);
    let (code, message, data) = start(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#));
    assert_eq!((code, message.as_str()), (-32017, "PrerequisiteFailed"));
    assert_eq!(data["blockedOn"], "job: AssignProcessToJobObject failed: denied");
    assert!(f.table.summaries().is_empty());
    f.host.prereq_spawn.store(false, Ordering::SeqCst);
    assert_eq!(f.start()["state"], "running", "the slot was released by the failures");
}

#[test]
fn echoed_flag_tokens_are_bounded() {
    let long = "x".repeat(5000);
    for tokens in [vec![long.clone()], vec!["-threads".to_string(), long.clone()], vec!["-nav".to_string(), "-nav".to_string()]] {
        let message = parse_flags(&tokens).unwrap_err();
        assert!(message.len() < 200, "{message}");
    }
    let repeated = parse_flags(&[long.clone(), long]).unwrap_err();
    assert!(repeated.len() < 200);
}

#[test]
fn a_fatal_error_ends_the_job_failed_after_the_grace() {
    let f = Fixture::new("fatal");
    f.start();
    let p = f.proc(0);
    p.write(b"loading
FATAL ERROR: cannot open thing
");
    let done = f.wait_state("j1-t", "failed");
    assert_eq!(done["fatal"], true);
    assert!(done["failLines"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("FATAL ERROR")));
    assert!(p.terminated(), "the lingering tree is ended");
    assert!(done.get("summary").is_none() && done.get("exitCode").is_none());

    // An OK line inside the grace wins, and the flag is still reported.
    let g = Fixture::new("fatal-then-ok");
    g.start();
    g.proc(0).write(b"FATAL ERROR: one input
 OK: 1 compiled, 0 failed, 0 skipped
");
    let done = g.wait_state("j1-t", "ok");
    assert_eq!(done["fatal"], true);
    let plain = Fixture::new("not-fatal");
    plain.start();
    assert_eq!(plain.status("j1-t")["fatal"], false);
}

#[test]
fn a_log_over_the_read_cap_is_read_to_the_end_when_the_process_exits() {
    let f = Fixture::new("big");
    f.start();
    let p = f.proc(0);
    let line = format!("{}
", "y".repeat(999));
    let mut file = std::fs::OpenOptions::new().append(true).open(&p.log).unwrap();
    for _ in 0..(9 * 1024) {
        file.write_all(line.as_bytes()).unwrap();
    }
    file.write_all(b" OK: 7 compiled, 0 failed, 0 skipped
").unwrap();
    drop(file);
    p.exit(0);
    let done = f.wait_state("j1-t", "ok");
    assert_eq!(done["summary"]["compiled"], 7);
}

#[test]
fn start_does_not_hold_the_table_while_it_spawns() {
    let f = Fixture::new("starting");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *f.host.hold.lock().unwrap() = Some((entered_tx, release_rx));
    std::thread::scope(|scope| {
        let starter = scope.spawn(|| f.table.start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))));
        entered_rx.recv_timeout(Duration::from_secs(5)).expect("spawn entered");
        // The table answers while the spawn is in flight ...
        assert_eq!(parse(&f.table.status(None).unwrap()), serde_json::json!({"jobs": []}));
        assert!(f.table.summaries().is_empty());
        // ... and a second start is refused busy.
        let (code, _, data) = err(f.table.start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))));
        assert_eq!(code, -32017);
        assert_eq!(data["blockedOn"], "job: another job is starting");
        // A shutdown that lands now must still end the job the starter then inserts.
        assert!(f.table.shutdown(Duration::from_secs(5)));
        release_tx.send(()).unwrap();
        let (code, _, data) = err(starter.join().unwrap());
        assert_eq!(code, -32017);
        assert_eq!(data["blockedOn"], "job: bridge is shutting down");
    });
    assert!(f.proc(0).terminated(), "the slot inserted after the shutdown was terminated");
    assert!(f.table.summaries().is_empty());
    f.table.reopen();
    assert_eq!(f.start()["jobId"], "j2-t", "the slot was released");
}

#[test]
fn ids_carry_the_load_nonce() {
    let f = Fixture::new("nonce");
    assert_eq!(f.start()["jobId"], "j1-t");
    let fresh = JobTable::new(f.host.clone(), Duration::from_millis(5));
    assert!(!fresh.nonce().is_empty() && fresh.nonce().bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
    assert_ne!(fresh.nonce(), "t");
}

#[test]
fn shutdown_ends_running_jobs_joins_watchers_and_refuses_starts_until_reopened() {
    let f = Fixture::new("shutdown");
    f.start();
    assert!(f.table.shutdown(Duration::from_secs(5)));
    assert!(f.proc(0).terminated());
    assert_eq!(f.status("j1-t")["state"], "cancelled");
    let (code, _, data) = err(f.table.start(Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))));
    assert_eq!(code, -32017);
    assert_eq!(data["blockedOn"], "job: bridge is shutting down");
    f.table.reopen();
    assert_eq!(f.start()["jobId"], "j2-t");
    assert!(f.table.shutdown(Duration::from_secs(5)));
}

#[test]
fn control_status_carries_the_jobs() {
    let f = Fixture::new("status");
    f.start();
    f.proc(0).write(b" OK: 1 compiled, 0 failed, 0 skipped\n");
    f.wait_state("j1-t", "ok");
    f.start();
    let summaries = f.table.summaries();
    let jobs: Vec<(&str, &str, &str)> = summaries.iter().map(|(id, kind, state)| (id.as_str(), *kind, *state)).collect();
    assert_eq!(jobs, [("j1-t", "resourcecompiler", "ok"), ("j2-t", "resourcecompiler", "running")]);
    let reply = cs2_bridge::control::status_object(&cs2_bridge::control::StatusInput {
        now: 1,
        queue: &[],
        listener: true,
        inflight_start: 0,
        revision: 1,
        shutting_down: false,
        jobs: &jobs,
    });
    let value = parse(&reply);
    assert_eq!(value["jobs"], serde_json::json!([{"id": "j1-t", "kind": "resourcecompiler", "state": "ok"}, {"id": "j2-t", "kind": "resourcecompiler", "state": "running"}]));
}

#[test]
fn the_rows_are_registered_and_lane_served() {
    use cs2_bridge::registry::{find, gate_with, Gate};
    for name in ["job.start", "job.status", "job.cancel"] {
        let row = find(name).unwrap();
        assert!(row.implemented && row.needs == 0 && row.blocked_on.is_none(), "{name}");
        assert!(cs2_bridge::dispatch::is_ported(name));
        assert_eq!(gate_with(name, 0, false), Gate::Run);
        assert!(job::serves(name));
    }
    assert!(!job::serves("job.list"));
}

/// The production wiring on a non-Windows host: the lane answers `job.*` itself.
#[cfg(not(windows))]
#[test]
fn linux_start_is_windows_only_and_status_cancel_answer_the_empty_table() {
    use cs2_bridge::ipc::{ControlReply, Route};
    use cs2_bridge::modules::ModuleViews;
    use cs2_bridge::registry::ModState;
    use cs2_bridge::wiring::BridgeState;

    struct Modules;
    impl ModuleViews for Modules {
        fn state(&self, _: u8) -> ModState {
            ModState::NotLoaded
        }
        fn is_verified(&self, _: u8) -> bool {
            false
        }
        fn revision(&self) -> u64 {
            1
        }
    }
    let bridge = BridgeState::new(Arc::new(Modules), Box::new(cs2_bridge::dispatch::is_ported));
    let reply = |method: &str, params: Option<&str>| match bridge.route_with_params(method, params) {
        Route::Reply(reply) => reply,
        Route::Queue => panic!("{method} must be answered on the lane, never queued"),
    };
    match reply("job.start", Some(&format!(r#"{{"kind":"resourcecompiler","input":"{INPUT}"}}"#))) {
        ControlReply::Error { code, message, data } => {
            assert_eq!((code, message.as_str()), (-32017, "PrerequisiteFailed"));
            assert_eq!(parse(&data.unwrap())["blockedOn"], "job: resourcecompiler is Windows-only");
        }
        other => panic!("{other:?}"),
    }
    match reply("job.status", None) {
        ControlReply::Result(result) => assert_eq!(parse(&result), serde_json::json!({"jobs": []})),
        other => panic!("{other:?}"),
    }
    match reply("job.cancel", Some(r#"{"jobId":"j1"}"#)) {
        ControlReply::Error { code, .. } => assert_eq!(code, -32602),
        other => panic!("{other:?}"),
    }
    assert!(bridge.status_object().contains("\"jobs\":[]"));
}
