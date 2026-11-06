//! `cargo bridge runtime` against a stub podman and a stub `install`: no game, no containers.
//!
//! The stub podman (`PODMAN`) records every argv (with the `XDG_RUNTIME_DIR` it got) and answers
//! `ps` / `inspect` from canned `STUB_*` values; its `exec` (vtmsend) records the payload, says
//! `creator version` and, for the bridge request, writes the Init line and the discovery file the
//! way the live bridge would. A fake bridge on a local TCP port answers `control.hello`, the
//! active-document methods and `control.capabilities`, so `open` and `status` run end to end.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_cs2-bridge-tools");

const STUB_PODMAN: &str = r#"#!/bin/bash
{ printf '%s' "$XDG_RUNTIME_DIR"; printf '\x1f%s' "$@"; printf '\n'; } >> "$STUB_DIR/calls"
case "$1" in
  ps)
    case "$*" in
      *"label=cs2mcp.tool=modeldoc"*) printf '%s' "${STUB_PS_MODELDOC:-}" ;;
      *"label=cs2mcp.tool "*) printf '%s' "${STUB_PS_TOOLS:-}" ;;
      *"name=^"*) printf '%s\n' "${STUB_PS_STATUS:-}" ;;
    esac
    exit "${STUB_PS_RC:-0}" ;;
  inspect)
    [ -n "${STUB_INSPECT_TOOL:-}" ] || { echo "no such container" >&2; exit 125; }
    printf '%s\n' "$STUB_INSPECT_TOOL" ;;
  exec)
    { cat "$STUB_APP/vtm_payload.kv3"; printf '%s\n' '----'; } >> "$STUB_DIR/payloads"
    if grep -q tool_request "$STUB_APP/vtm_payload.kv3"; then
      printf '[cs2-bridge mm] Init\n[cs2-bridge mm+tool] Init\n' >> "$STUB_APP/mcp_bridge_rs.log"
      d="$STUB_APP/compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge"
      mkdir -p "$d" && printf '%s' "$STUB_DISC" > "$d/session.json"
    fi
    echo "vtmsend: creator version 1" ;;
  run) echo 0123abcd ;;
esac
exit 0
"#;

const STUB_INSTALL: &str = r#"#!/bin/bash
printf '%s|%s|%s|%s\n' "$*" "$CS2_MCP_PROJECT_ROOT" "$CS2_GAME" "$XDG_RUNTIME_DIR" >> "$STUB_DIR/install.calls"
exit "${STUB_INSTALL_RC:-0}"
"#;

/// The script's knobs: cleared so the developer's environment cannot leak into a test.
const KNOBS: &[&str] = &[
    "CONTAINER",
    "IMG",
    "CS2_MCP_IMAGE",
    "ADDON",
    "POC_ASSET",
    "WAIT_CHANNEL_S",
    "WAIT_DISCOVERY_S",
    "WAIT_DOC_S",
    "TOOL",
    "EXTRA_ARGS",
    "CS2MCP_TESTPLUGIN_MODE",
    "CS2MCP_TESTPLUGIN_LOG",
    "CS2DOCKER_ROOT",
    "STUB_PS_MODELDOC",
    "STUB_PS_TOOLS",
    "STUB_PS_STATUS",
    "STUB_PS_RC",
    "STUB_INSPECT_TOOL",
    "STUB_INSTALL_RC",
];

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    app: PathBuf,
    stubs: PathBuf,
    xauth: PathBuf,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("runtime-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let root = dir.path().join("project");
        let app = root.join("workshop/run/app");
        let stubs = dir.path().join("stubs");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&stubs).unwrap();
        script(&stubs.join("podman"), STUB_PODMAN);
        script(&stubs.join("install"), STUB_INSTALL);
        let xauth = stubs.join("xauth");
        std::fs::write(&xauth, "").unwrap();
        Fixture {
            _dir: dir,
            root,
            app,
            stubs,
            xauth,
        }
    }

    fn disc(&self) -> PathBuf {
        self.app.join(
            "compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json",
        )
    }

    /// `cargo bridge runtime <args>` with the stubs, `env` on top.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.arg("runtime").args(args);
        for knob in KNOBS {
            cmd.env_remove(knob);
        }
        cmd.env("PODMAN", self.stubs.join("podman"))
            .env("CS2MCP_RUNTIME_INSTALL_EXE", self.stubs.join("install"))
            .env("CS2_MCP_PROJECT_ROOT", &self.root)
            .env("XAUTHORITY", &self.xauth)
            .env("STUB_DIR", &self.stubs)
            .env("STUB_APP", &self.app)
            .env("STUB_DISC", "{}");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// Every podman call: `(XDG_RUNTIME_DIR, argv)`.
    fn calls(&self) -> Vec<(String, Vec<String>)> {
        let text = std::fs::read_to_string(self.stubs.join("calls")).unwrap_or_default();
        text.lines()
            .map(|line| {
                let mut fields = line.split('\x1f').map(String::from);
                let xdg = fields.next().unwrap();
                (xdg, fields.collect())
            })
            .collect()
    }

    fn argvs(&self, verb: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .map(|(_, a)| a)
            .filter(|a| a[0] == verb)
            .collect()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.stubs.join(name)).unwrap_or_default()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A fake bridge: answers hello (checking the token), the two active-document methods and the
/// capabilities. Returns its discovery text and the methods it saw.
fn fake_bridge(modules: Value) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            let modules = modules.clone();
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { return };
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_string();
                    log.lock().unwrap().push(method.clone());
                    let reply = match method.as_str() {
                        "control.hello" if req["params"]["token"] == "tok" => {
                            json!({"result": {"protocol": 1, "bridgeVersion": "m9.9", "gameBuild": "g1"}})
                        }
                        "control.hello" => {
                            json!({"error": {"code": -32001, "message": "bad token"}})
                        }
                        "doc.active" | "modeldoc.active" => {
                            json!({"result": {"doc": {"id": "doc1"}}})
                        }
                        "control.capabilities" => json!({"result": {"modules": modules}}),
                        _ => json!({"error": {"code": -32601}}),
                    };
                    let mut reply = reply;
                    reply["jsonrpc"] = json!("2.0");
                    reply["id"] = req["id"].clone();
                    if writer.write_all(format!("{reply}\n").as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    let disc = format!("{{\n  \"pid\": 1,\n  \"host\": \"127.0.0.1\",\n  \"port\": {port},\n  \"token\": \"tok\"\n}}\n");
    (disc, seen)
}

fn verified(names: &[&str]) -> Value {
    names
        .iter()
        .map(|n| (n.to_string(), json!({"state": "verified"})))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

#[test]
fn up_defaults() {
    let fx = Fixture::new();
    std::fs::write(
        fx.app.join("mcp_bridge_rs.log"),
        "[cs2-bridge mm+tool] Init (last session)\n",
    )
    .unwrap();
    std::fs::create_dir_all(fx.disc().parent().unwrap()).unwrap();
    std::fs::write(fx.disc(), "stale").unwrap();
    let out = fx.run(&["up"], &[]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("added -insecure to EXTRA_ARGS"), "{err}");
    assert!(err.contains("container cs2tools removed"), "{err}");
    assert!(err.contains("channel up after"), "{err}");
    assert!(err.contains("rust bridge Init after"), "{err}");
    assert!(err.contains("discovery file after"), "{err}");

    let xdg = fx.root.join(".podman/xdg").display().to_string();
    let calls = fx.calls();
    assert!(calls.iter().all(|(x, _)| *x == xdg), "{calls:?}");
    let verbs: Vec<&str> = calls.iter().map(|(_, a)| a[0].as_str()).collect();
    // our own container is checked for being the ModelDoc instance; a hammer up runs no ModelDoc ps
    assert_eq!(&verbs[..3], ["inspect", "rm", "run"], "{verbs:?}");
    assert!(verbs[3..].iter().all(|v| *v == "exec"), "{verbs:?}");
    assert_eq!(calls[1].1, ["rm", "-f", "cs2tools"]);

    let app = fx.app.display().to_string();
    let cache = fx.root.join("workshop/run/cache").display().to_string();
    #[rustfmt::skip]
    let want: Vec<String> = [
        "run", "-d", "--gpus", "all", "--name", "cs2tools", "--network=host",
        "--label", "cs2mcp.tool=hammer", "--label", "cs2mcp.addon=mcp_test",
        "-v", &format!("{app}:/app"), "-v", &format!("{cache}:/cache"),
        "-v", "/tmp/.X11-unix:/tmp/.X11-unix", "-v", &format!("{}:/root/.Xauthority:ro", fx.xauth.display()),
        "-e", "DISPLAY=:0", "-e", "XAUTHORITY=/root/.Xauthority", "--entrypoint", "/bin/bash",
        "ghcr.io/personcopperfine/cs2-docker:latest", "-c",
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug -addon mcp_test -insecure \
         +engine_no_focus_sleep 0 +r_always_render_all_windows 1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(calls[2].1, want);
    assert_eq!(
        fx.argvs("exec")[0],
        [
            "exec",
            "-i",
            "cs2tools",
            "bash",
            "-c",
            "cd /app && ./entrypoint.sh vtmsend.exe 2>&1"
        ]
    );

    // install ensure ran with the script's environment
    let game = fx.app.join("game").display().to_string();
    assert_eq!(
        fx.read("install.calls"),
        format!(
            "install --platform wine ensure|{}|{game}|{xdg}\n",
            fx.root.display()
        )
    );
    // the channel probe, then the bridge request
    let payloads = fx.read("payloads");
    let mut posts = payloads.split("----\n").filter(|p| !p.is_empty());
    assert_eq!(
        posts.next().unwrap(),
        cs2_vtm::command("echo mcp_runtime_channel_probe")
    );
    assert_eq!(posts.next().unwrap(), cs2_vtm::bridge_request());
    // the old log is rotated (so the old Init line does not count) and the stale discovery replaced
    let prev = std::fs::read_to_string(fx.app.join("mcp_bridge_rs.log.prev")).unwrap();
    assert!(prev.contains("last session"));
    assert_eq!(std::fs::read_to_string(fx.disc()).unwrap(), "{}");
}

#[test]
fn up_empty_addon_and_own_insecure() {
    let fx = Fixture::new();
    let out = fx.run(
        &["up"],
        &[
            ("ADDON", ""),
            ("EXTRA_ARGS", "-novid -insecure +sv_cheats 1"),
            ("IMG", "local/x:1"),
        ],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("added -insecure"), "{err}");
    assert!(err.contains("addon='none'"), "{err}");
    let run = &fx.argvs("run")[0];
    assert!(
        !run.iter().any(|a| a.starts_with("cs2mcp.addon")),
        "{run:?}"
    );
    assert_eq!(run[run.len() - 2..run.len() - 1], ["-c"]);
    assert_eq!(run[run.len() - 3], "local/x:1");
    assert_eq!(
        run.last().unwrap(),
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug  -novid -insecure \
         +sv_cheats 1 +engine_no_focus_sleep 0 +r_always_render_all_windows 1"
    );
}

#[test]
fn up_extra_args_flag_and_image_fallback() {
    let fx = Fixture::new();
    let out = fx.run(
        &["up", "--extra-args", "-novid", "--addon", "a2"],
        &[("CS2_MCP_IMAGE", "local/y:2")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let run = &fx.argvs("run")[0];
    assert!(run.contains(&"cs2mcp.addon=a2".to_string()), "{run:?}");
    assert_eq!(run[run.len() - 3], "local/y:2");
    assert!(run
        .last()
        .unwrap()
        .contains("-condebug -addon a2 -novid -insecure +engine_no_focus_sleep 0"));
}

#[test]
fn up_modeldoc_labels_and_testplugin_env() {
    let fx = Fixture::new();
    let out = fx.run(
        &["up", "--container", "md1"],
        &[
            ("TOOL", "modeldoc"),
            ("CS2MCP_TESTPLUGIN_MODE", "coexist"),
            ("CS2MCP_TESTPLUGIN_LOG", ""),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let ps = fx.argvs("ps");
    assert_eq!(
        ps[0],
        [
            "ps",
            "--filter",
            "label=cs2mcp.tool=modeldoc",
            "--format",
            "{{.Names}}"
        ]
    );
    let run = &fx.argvs("run")[0];
    let at = run.iter().position(|a| a == "--label").unwrap();
    assert_eq!(
        run[at..at + 6],
        [
            "--label",
            "cs2mcp.tool=modeldoc",
            "--label",
            "cs2mcp.addon=mcp_test",
            "-e",
            "CS2MCP_TESTPLUGIN_MODE=coexist"
        ]
    );
    assert_eq!(
        run[at + 6],
        "-v",
        "an empty CS2MCP_TESTPLUGIN_LOG is not passed: {run:?}"
    );
    assert_eq!(run[5], "md1");
}

#[test]
fn up_modeldoc_refused_while_one_runs() {
    let fx = Fixture::new();
    let out = fx.run(
        &["up", "--tool", "modeldoc"],
        &[("STUB_PS_MODELDOC", "other-md\n")],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("a ModelDoc instance is already running (other-md); only one at a time"),
        "{err}"
    );
    assert!(fx.argvs("rm").is_empty() && fx.argvs("run").is_empty());
    assert_eq!(fx.read("install.calls"), "", "no install while refused");
}

#[test]
fn up_modeldoc_refused_when_podman_ps_fails() {
    let fx = Fixture::new();
    let out = fx.run(&["up"], &[("TOOL", "modeldoc"), ("STUB_PS_RC", "125")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("podman ps failed; cannot check for a running ModelDoc instance"));
    assert!(fx.argvs("run").is_empty());
}

#[test]
fn up_never_replaces_the_running_modeldoc() {
    let fx = Fixture::new();
    // a hammer up whose container name is the running ModelDoc instance
    let out = fx.run(
        &["up"],
        &[("CONTAINER", "md"), ("STUB_INSPECT_TOOL", "modeldoc")],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("md is the running ModelDoc instance; stop it explicitly first"),
        "{err}"
    );
    assert!(fx.argvs("rm").is_empty());
}

#[test]
fn up_bad_tool() {
    let fx = Fixture::new();
    let out = fx.run(&["up"], &[("TOOL", "pet")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("TOOL must be hammer or modeldoc (got 'pet')"));
    assert!(fx.calls().is_empty());
}

#[test]
fn up_stops_when_install_refuses() {
    let fx = Fixture::new();
    let out = fx.run(&["up"], &[("STUB_INSTALL_RC", "4")]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("install ensure failed (rc 4) - not starting"),
        "{err}"
    );
    assert!(fx.argvs("run").is_empty());
}

#[test]
fn up_without_xauthority() {
    let fx = Fixture::new();
    let out = fx.run(&["up"], &[("XAUTHORITY", "/nonexistent/xauth")]);
    // the /run/user fallback may find the developer's own file; either way no bad mount
    if !out.status.success() {
        assert!(stderr(&out).contains("no XAUTHORITY file found"));
        assert!(fx.argvs("run").is_empty());
    } else {
        let run = &fx.argvs("run")[0];
        assert!(
            !run.iter().any(|a| a.starts_with("/nonexistent")),
            "{run:?}"
        );
    }
}

#[test]
fn up_clears_pet_flags_only_when_alone() {
    let fx = Fixture::new();
    let settings = fx.app.join("game/_toolsettings/pet/settings.txt");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "\"Closed Correctly\" = false\n").unwrap();

    let out = fx.run(&["up"], &[("STUB_PS_TOOLS", "other\n")]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("WARNING: pet's settings flag an unclean exit"));
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        "\"Closed Correctly\" = false\n"
    );

    // our own (just removed) container does not count as another one
    let out = fx.run(&["up"], &[("STUB_PS_TOOLS", "cs2tools\n")]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("pet: cleared the unclean-exit flags"));
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        "\"Closed Correctly\" = true\n"
    );
}

#[test]
fn up_times_out_without_init() {
    let fx = Fixture::new();
    // the stub only writes the Init line for a bridge request; make it write nothing
    let podman = fx.stubs.join("podman");
    let body = STUB_PODMAN.replace(
        "if grep -q tool_request",
        "if false && grep -q tool_request",
    );
    script(&podman, &body);
    let out = fx.run(&["up", "--wait-discovery-s", "1"], &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("rust bridge never logged its tool-role Init"),
        "{err}"
    );
    // posted at once and not again within the second
    assert_eq!(fx.read("payloads").matches("tool_request").count(), 1);
}

#[test]
fn down_with_container() {
    let fx = Fixture::new();
    let out = fx.run(&["down"], &[("CONTAINER", "mine")]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("container mine removed"));
    let calls = fx.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0,
        fx.root.join(".podman/xdg").display().to_string()
    );
    assert_eq!(calls[0].1, ["rm", "-f", "mine"]);
    // the flag wins over the variable
    fx.run(
        &["down", "--container", "flagged"],
        &[("CONTAINER", "mine")],
    );
    assert_eq!(fx.calls()[1].1, ["rm", "-f", "flagged"]);
}

#[test]
fn status_without_discovery() {
    let fx = Fixture::new();
    let out = fx.run(&["status"], &[("STUB_PS_STATUS", "Up 3 minutes")]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(err, "container: Up 3 minutes\ndiscovery: absent\n");
    assert_eq!(
        fx.calls()[0].1,
        [
            "ps",
            "--filter",
            "name=^cs2tools$",
            "--format",
            "{{.Status}}"
        ]
    );
}

#[test]
fn status_live() {
    let fx = Fixture::new();
    let (disc, seen) = fake_bridge(json!({}));
    std::fs::create_dir_all(fx.disc().parent().unwrap()).unwrap();
    std::fs::write(fx.disc(), disc).unwrap();
    let out = fx.run(&["status"], &[("STUB_PS_STATUS", "Up 1 second")]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines[0], "container: Up 1 second");
    assert!(
        lines[1].starts_with("discovery: {\"pid\":1,\"host\":\"127.0.0.1\",\"port\":"),
        "{err}"
    );
    assert_eq!(lines[2], "bridge: alive (bridgeVersion m9.9, gameBuild g1)");
    assert_eq!(*seen.lock().unwrap(), ["control.hello"]);
}

#[test]
fn status_bridge_gone() {
    let fx = Fixture::new();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::create_dir_all(fx.disc().parent().unwrap()).unwrap();
    std::fs::write(
        fx.disc(),
        format!("{{\"host\":\"127.0.0.1\",\"port\":{port},\"token\":\"t\"}}"),
    )
    .unwrap();
    let out = fx.run(&["status"], &[]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("bridge: not reachable"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn open_model_refused_outside_modeldoc() {
    let fx = Fixture::new();
    for asset in ["models/x.vmdl", "models/x.vmdl_c"] {
        let out = fx.run(&["open", asset], &[("STUB_INSPECT_TOOL", "hammer")]);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(
            err.contains(&format!(
                "{asset} is a model: open it in the ModelDoc instance"
            )),
            "{err}"
        );
    }
    assert!(fx.argvs("exec").is_empty());
    assert!(!fx.app.join("vtm_payload.kv3").exists());
}

#[test]
fn open_map_waits_for_document_and_modules() {
    let fx = Fixture::new();
    let (disc, seen) = fake_bridge(verified(&["tier0.dll", "hammer.dll"]));
    std::fs::create_dir_all(fx.disc().parent().unwrap()).unwrap();
    std::fs::write(fx.disc(), disc).unwrap();
    let out = fx.run(
        &["open"],
        &[
            ("STUB_INSPECT_TOOL", "hammer"),
            ("POC_ASSET", "maps/x.vmap"),
        ],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("open_asset maps/x.vmap"), "{err}");
    assert!(err.contains("document open after"), "{err}");
    assert!(
        err.contains("modules: hammer.dll=verified,tier0.dll=verified"),
        "{err}"
    );
    assert_eq!(
        fx.read("payloads"),
        format!("{}----\n", cs2_vtm::open_asset("maps/x.vmap"))
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "control.hello",
            "doc.active",
            "control.hello",
            "control.capabilities"
        ]
    );
}

#[test]
fn open_model_in_modeldoc() {
    let fx = Fixture::new();
    let (disc, seen) = fake_bridge(verified(&["tier0.dll", "modeldoc_editor.dll"]));
    std::fs::create_dir_all(fx.disc().parent().unwrap()).unwrap();
    std::fs::write(fx.disc(), disc).unwrap();
    let out = fx.run(
        &["open", "models/x.vmdl"],
        &[("STUB_INSPECT_TOOL", "modeldoc")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(seen
        .lock()
        .unwrap()
        .contains(&"modeldoc.active".to_string()));
}

#[test]
fn open_times_out_without_a_bridge() {
    let fx = Fixture::new();
    let out = fx.run(&["open", "maps/x.vmap", "--wait-doc-s", "1"], &[]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("warn: no document after 1s"), "{err}");
}

#[test]
fn cycle_is_up_then_open() {
    let fx = Fixture::new();
    let (disc, seen) = fake_bridge(verified(&["tier0.dll", "hammer.dll"]));
    let mut cmd_env = vec![("STUB_INSPECT_TOOL", "hammer")];
    cmd_env.push(("STUB_DISC", disc.as_str()));
    // the container is hammer-labelled for open, which up must still accept
    let out = fx.run(&["cycle", "maps/c.vmap"], &cmd_env);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("open_asset maps/c.vmap"), "{err}");
    assert!(err.ends_with("cycle complete (rebuild + reinstall first: cargo bridge build, cargo bridge install)\n"), "{err}");
    assert!(seen.lock().unwrap().contains(&"doc.active".to_string()));
}

#[test]
fn logs_tail_both() {
    let fx = Fixture::new();
    std::fs::write(fx.app.join("mcp_bridge_rs.log"), "a\nb\nc\n").unwrap();
    let out = fx.run(&["logs", "2"], &[]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b\nc\n");
    assert_eq!(
        stderr(&out),
        "=== bridge log (last 2) ===\n=== game console (last 2) ===\n(no console log)\n"
    );
}

#[test]
fn bad_wait_value() {
    let fx = Fixture::new();
    let out = fx.run(&["up"], &[("WAIT_CHANNEL_S", "soon")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("WAIT_CHANNEL_S must be a number of seconds (got 'soon')"));
}
