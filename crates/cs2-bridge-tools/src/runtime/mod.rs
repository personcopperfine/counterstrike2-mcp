//! `cargo bridge runtime`: the bridge reload/inspection cycle for the Wine tools runtime (the
//! port of the shell script `bridge/runtime.sh`, in history as `git show 8cdea42:bridge/runtime.sh`).
//!
//! The bridge is the Rust crate `crates/cs2-bridge` (`cs2_bridge.dll`): `cargo bridge build`
//! builds it, `cargo bridge install --platform wine ensure` puts it into the shared tools
//! install, and `up` runs that ensure itself (read-only when current, an install when stale).
//! The `sdkenginetools.txt` manifest entry is read at toolframework2 Init, so a fresh install
//! only takes effect after a full runtime restart, i.e. `up`.
//!
//! One ModelDoc instance at a time (AGENTS.md rule 7): `up --tool modeldoc` refuses while any
//! container labelled `cs2mcp.tool=modeldoc` runs, checked under a lock on
//! `<project root>/.podman/cs2mcp-modeldoc.lock` (the script's flock file; `File::lock` is
//! `flock(2)` on Linux, so the two interlock) so two `up`s cannot both pass. No `up` replaces a
//! running ModelDoc container, ours or another agent's: stop it on purpose with
//! `cargo bridge runtime down --container <name>`. `open <x>.vmdl` only runs in a ModelDoc
//! container.
//!
//! Every knob keeps the script's environment variable as its fallback (`CONTAINER`, `IMG`,
//! `ADDON`, `TOOL`, `EXTRA_ARGS`, ...), so `TOOL=modeldoc ADDON=x cargo bridge runtime up` works
//! as `TOOL=modeldoc ADDON=x bridge/runtime.sh up` did. As in the script, an empty value means
//! the default, except `ADDON`, where empty disables `-addon`.
//!
//! A modal in Hammer stops the main thread; if `up`/`open` times out, check `logs` and take a
//! screenshot (`spectacle -b -n -f -o shot.png`) for a stalled dialog.

mod podman;
mod rpc;

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;

use podman::{Launch, Podman, Tool, TOOL_LABEL};

/// The command line of `cargo bridge runtime`.
#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(subcommand)]
    // boxed: the up/open knobs would make every `cargo bridge` command this size
    command: Box<Cmd>,
    #[command(flatten)]
    common: Common,
}

#[derive(clap::Args, Debug)]
struct Common {
    /// The project root holding workshop/run/app and .podman/ [default: $CS2_MCP_PROJECT_ROOT,
    /// else the main checkout of this repo]
    #[arg(long, global = true, display_order = 100, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// The tools container
    #[arg(long, global = true, display_order = 101, env = "CONTAINER", value_name = "NAME", default_value = DEFAULT_CONTAINER)]
    container: String,
    /// The podman program (a stub in the tests)
    #[arg(
        long,
        global = true,
        display_order = 102,
        env = "PODMAN",
        value_name = "PROGRAM",
        default_value = "podman"
    )]
    podman: OsString,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Fresh session: restart the runtime, load the bridge, wait for discovery
    Up(UpOpts),
    /// Post open_asset and wait for the document and the module checks
    Open {
        /// The asset, content-relative [default: --poc-asset]
        asset: Option<String>,
        #[command(flatten)]
        open: OpenOpts,
    },
    /// up -> open (build + install first: cargo bridge build, cargo bridge install)
    Cycle {
        /// The asset, content-relative [default: --poc-asset]
        asset: Option<String>,
        #[command(flatten)]
        up: UpOpts,
        #[command(flatten)]
        open: OpenOpts,
    },
    /// Container / discovery / liveness summary
    Status,
    /// Tail the bridge log and the game console
    Logs {
        /// Lines of each
        #[arg(default_value_t = 40)]
        lines: usize,
    },
    /// Stop (remove) the container
    Down,
}

#[derive(clap::Args, Debug)]
struct UpOpts {
    /// The client image the Wine tools run in [default: $CS2_MCP_IMAGE, else
    /// ghcr.io/personcopperfine/cs2-docker:latest]; a custom build from a clone of
    /// personcopperfine/cs2-docker (`podman build -t <tag> <clone>/client`) goes here
    #[arg(long, env = "IMG", value_name = "IMAGE")]
    image: Option<String>,
    /// The image when --image/IMG is unset (the MCP servers' variable)
    #[arg(long, env = "CS2_MCP_IMAGE", value_name = "IMAGE")]
    cs2_mcp_image: Option<String>,
    /// The addon (`-addon <name>`, label cs2mcp.addon); an empty value starts without one
    #[arg(long, env = "ADDON", default_value = DEFAULT_ADDON)]
    addon: String,
    /// The tool: hammer | modeldoc (labels the container cs2mcp.tool=<tool>)
    #[arg(long, env = "TOOL", default_value = "hammer")]
    tool: String,
    /// Extra cs2.exe arguments, verbatim (the launcher passes them on); -insecure is added
    /// unless present, since Metamod:Source refuses to load without it
    #[arg(
        long,
        env = "EXTRA_ARGS",
        allow_hyphen_values = true,
        default_value = ""
    )]
    extra_args: String,
    /// The coexistence fixture's mode (crates/mm-testplugin), passed into the container when set
    #[arg(long, env = "CS2MCP_TESTPLUGIN_MODE")]
    testplugin_mode: Option<String>,
    /// The coexistence fixture's log path, passed into the container when set
    #[arg(long, env = "CS2MCP_TESTPLUGIN_LOG")]
    testplugin_log: Option<String>,
    /// Seconds to wait for the VToolMessage channel (engine start)
    #[arg(long, env = "WAIT_CHANNEL_S", default_value = "180", value_name = "S")]
    wait_channel_s: String,
    /// Seconds to wait for the bridge's Init line, then again for its discovery file (the load
    /// measured 22-34 s)
    #[arg(long, env = "WAIT_DISCOVERY_S", default_value = "90", value_name = "S")]
    wait_discovery_s: String,
    /// The program `up` runs as `<exe> install --platform wine ensure` [default: this binary]
    #[arg(long, env = "CS2MCP_RUNTIME_INSTALL_EXE", hide = true)]
    install_exe: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct OpenOpts {
    /// The asset `open`/`cycle` post when none is given
    #[arg(long, env = "POC_ASSET", default_value = DEFAULT_POC_ASSET, value_name = "ASSET")]
    poc_asset: String,
    /// Seconds to wait for the document to open
    #[arg(long, env = "WAIT_DOC_S", default_value = "60", value_name = "S")]
    wait_doc_s: String,
}

const DEFAULT_CONTAINER: &str = "cs2tools";
const DEFAULT_IMAGE: &str = "ghcr.io/personcopperfine/cs2-docker:latest";
const DEFAULT_ADDON: &str = "mcp_test";
const DEFAULT_POC_ASSET: &str = "maps/probe.vmap";
/// The module checks get a fixed minute (the script had no knob for it).
const WAIT_MODULES: Duration = Duration::from_secs(60);
/// How long `up` waits for another `up` to release the ModelDoc lock (`flock -w 30`).
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// `vtmsend` is a `podman exec` that starts Wine: once a second is plenty.
const VTM_POLL: Duration = Duration::from_secs(1);
/// Local files (the log, the discovery file) are cheap to look at.
const FILE_POLL: Duration = Duration::from_millis(250);
/// The bridge request is re-posted this often until its Init line shows.
const BRIDGE_REPOST: Duration = Duration::from_secs(5);
/// What vtmsend prints once toolframework2 Init created the channel; before that it exits 1
/// with `OpenMutex failed` (`cs2_mcp_common::runtime::readiness::CHANNEL_UP_MARKER`).
const CHANNEL_UP: &str = "creator version";
/// The tool role's Init line. The bridge's log tag is a role set `[cs2-bridge <roles>]` with
/// roles from {mm, tool} joined by `+` (crates/cs2-bridge/src/log.rs): the tool copy logs
/// `[cs2-bridge mm+tool] Init` when it is both, old builds `[cs2-bridge tool] Init`. The
/// Metamod copy writes to the same file, and a bare `[cs2-bridge mm] Init` proves nothing about
/// the tool, so it never matches.
const RS_INIT_RE: &str = r"\[cs2-bridge (mm\+)?tool\] Init";

/// An empty environment value means the default (the script's `${X:-default}`).
fn or_default(value: &str, default: &str) -> String {
    if value.is_empty() { default } else { value }.to_string()
}

fn seconds(value: &str, default: u64, name: &str) -> Result<Duration> {
    if value.is_empty() {
        return Ok(Duration::from_secs(default));
    }
    let secs: u64 = value
        .trim()
        .parse()
        .with_context(|| format!("{name} must be a number of seconds (got '{value}')"))?;
    Ok(Duration::from_secs(secs))
}

/// The files under the project root the runtime reads and writes.
struct Paths {
    project: PathBuf,
    app: PathBuf,
    cache: PathBuf,
    /// The bridge's discovery file inside the Wine prefix (`%LOCALAPPDATA%\cs2mcpbridge`).
    disc: PathBuf,
    /// The fixed file vtmsend.exe posts (`Z:\app\vtm_payload.kv3`).
    payload: PathBuf,
    /// The Rust bridge's log file (crates/cs2-bridge/src/log.rs, the Windows path).
    rs_log: PathBuf,
    console_log: PathBuf,
    pet_settings: PathBuf,
    modeldoc_lock: PathBuf,
}

impl Paths {
    fn new(project: PathBuf) -> Self {
        let app = project.join("workshop/run/app");
        let disc = app.join(
            "compatdata/730/pfx/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json",
        );
        Paths {
            cache: project.join("workshop/run/cache"),
            payload: app.join("vtm_payload.kv3"),
            rs_log: app.join("mcp_bridge_rs.log"),
            console_log: app.join("game/csgo/console.log"),
            pet_settings: app.join("game/_toolsettings/pet/settings.txt"),
            modeldoc_lock: project.join(".podman/cs2mcp-modeldoc.lock"),
            disc,
            app,
            project,
        }
    }
}

struct Ctx {
    paths: Paths,
    container: String,
    podman: Podman,
}

fn say(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Runs the command; `Ok` is the exit code.
pub fn run(args: Args) -> Result<u8> {
    let project = args
        .common
        .project_root
        .clone()
        .unwrap_or_else(crate::root::project_root);
    let ctx = Ctx {
        podman: Podman {
            program: args.common.podman.clone(),
            xdg: crate::root::podman_xdg(&project),
        },
        container: or_default(&args.common.container, DEFAULT_CONTAINER),
        paths: Paths::new(project),
    };
    match *args.command {
        Cmd::Up(up) => ctx.up(&up).map(|()| 0),
        Cmd::Open { asset, open } => ctx.open(asset.as_deref(), &open),
        Cmd::Cycle { asset, up, open } => {
            ctx.up(&up)?;
            let code = ctx.open(asset.as_deref(), &open)?;
            if code == 0 {
                say("cycle complete (rebuild + reinstall first: cargo bridge build, cargo bridge install)");
            }
            Ok(code)
        }
        Cmd::Status => ctx.status().map(|()| 0),
        Cmd::Logs { lines } => {
            ctx.logs(lines);
            Ok(0)
        }
        Cmd::Down => {
            ctx.down();
            Ok(0)
        }
    }
}

/// Polls `check` until it yields or `deadline` passes, `interval` apart (the last wait is cut
/// to the deadline). `check` runs at least once, and once more at the deadline.
fn poll<T>(
    deadline: Instant,
    interval: Duration,
    mut check: impl FnMut() -> Option<T>,
) -> Option<T> {
    loop {
        if let Some(v) = check() {
            return Some(v);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        sleep(interval.min(deadline - now));
    }
}

/// Takes the exclusive lock on `path` (created if missing), waiting up to `wait` for another
/// holder. The lock goes with the returned `File`; Rust opens files close-on-exec, so neither
/// podman nor the container inherits it.
fn lock_file(path: &Path, wait: Duration) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                sleep(Duration::from_millis(100))
            }
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "timed out waiting for {} (another `runtime up` holds it)",
                    path.display()
                )
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

/// pet's unclean-exit flags set back to `true` (`"Closed Correctly" = false`, also the
/// `ClosedCorrectly` and numbered spellings), or `None` when none is set.
fn clear_closed_flags(text: &str) -> Option<String> {
    // `[ \t]` rather than `\s`: the flag and its value sit on one line, as sed saw them
    let re = regex::Regex::new(r#"("Closed ?Correctly( [0-9]+)?"[ \t]*=[ \t]*)false"#)
        .expect("valid regex");
    re.is_match(text)
        .then(|| re.replace_all(text, "${1}true").into_owned())
}

/// The last `n` lines of `text`.
fn tail(text: &str, n: usize) -> &str {
    if n == 0 {
        return "";
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    match body.match_indices('\n').rev().nth(n - 1) {
        Some((at, _)) => &text[at + 1..],
        None => text,
    }
}

/// The X authority file to mount: `$XAUTHORITY` when it is a file, else the newest
/// `/run/user/<uid>/xauth_*` (where the desktop session puts it).
fn xauth_file() -> Option<PathBuf> {
    if let Some(xa) = std::env::var_os("XAUTHORITY").map(PathBuf::from) {
        if xa.is_file() {
            return Some(xa);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self").ok()?.uid();
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(format!("/run/user/{uid}"))
            .ok()?
            .flatten()
        {
            if !entry.file_name().to_string_lossy().starts_with("xauth_") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, entry.path()));
            }
        }
        newest.map(|(_, path)| path)
    }
    #[cfg(not(unix))]
    None
}

impl Ctx {
    fn is_modeldoc(&self) -> bool {
        self.podman.container_tool(&self.container) == Tool::Modeldoc.as_str()
    }

    /// Writes the payload vtmsend.exe posts, through a temp file and a rename so a vtmsend
    /// still running never reads half of it.
    fn put_payload(&self, payload: &str) -> Result<()> {
        let tmp = self.paths.payload.with_extension("kv3.tmp");
        std::fs::write(&tmp, payload).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.paths.payload)
            .with_context(|| format!("writing {}", self.paths.payload.display()))
    }

    /// Posts the current payload; `Ok((success, output))`.
    fn vtmsend(&self) -> Result<(bool, String)> {
        let out = self
            .podman
            .output(&podman::vtmsend_argv(&self.container))
            .context("running podman exec")?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), text))
    }

    fn down(&self) {
        let _ = self
            .podman
            .command(&podman::rm_argv(&self.container))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        say(format!("container {} removed", self.container));
    }

    /// One ModelDoc instance at a time; the caller holds the lock.
    fn guard_modeldoc(&self) -> Result<()> {
        let argv = podman::ps_names_argv(&format!("{TOOL_LABEL}=modeldoc"));
        let out = self
            .podman
            .output(&argv)
            .ok()
            .filter(|o| o.status.success());
        let Some(out) = out else {
            bail!("podman ps failed; cannot check for a running ModelDoc instance")
        };
        let running: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(String::from)
            .collect();
        if !running.is_empty() {
            bail!(
                "a ModelDoc instance is already running ({}); only one at a time - stop it explicitly: \
                 cargo bridge runtime down --container <name>",
                running.join(" ")
            );
        }
        Ok(())
    }

    /// pet's crash recovery: a pet session that did not exit cleanly (every killed or wedged
    /// runtime) leaves `"Closed Correctly" = false` in `game/_toolsettings/pet/settings.txt`,
    /// and the next pet.open then blocks the main thread on the modal "Recover Files?". The
    /// autosaves (`content/_toolsautosave/pet/`) are kept; only the flags are reset, and only
    /// when no other tools container shares the tree (it could have pet open and rewrite the
    /// file). A failing `podman ps` reads as "no other container": the same as a fresh machine,
    /// and the only container we know of was just stopped.
    fn clear_pet_crash_flags(&self) -> Result<()> {
        let settings = &self.paths.pet_settings;
        let Ok(text) = std::fs::read_to_string(settings) else {
            return Ok(());
        };
        let Some(cleared) = clear_closed_flags(&text) else {
            return Ok(());
        };
        let others: Vec<String> = match self.podman.output(&podman::ps_names_argv(TOOL_LABEL)) {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .filter(|name| *name != self.container)
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        };
        if !others.is_empty() {
            let others = others.join(" ");
            say(format!(
                "WARNING: pet's settings flag an unclean exit but other tools containers share the tree ({others});"
            ));
            say(format!(
                "WARNING: flags left as is - the first pet.open in {} will block on the modal 'Recover Files?'.",
                self.container
            ));
            say(format!(
                "WARNING: stop those containers (or reset {} by hand while no pet runs) before opening particles.",
                settings.display()
            ));
            return Ok(());
        }
        std::fs::write(settings, cleared)
            .with_context(|| format!("writing {}", settings.display()))?;
        say("pet: cleared the unclean-exit flags (no Recover Files? modal); autosaves kept in content/_toolsautosave/pet/");
        Ok(())
    }

    /// `install --platform wine ensure` with the environment the servers' launch ensure gets: a read-only
    /// check, repaired by an install, which refuses (exit 4) while ANOTHER session's container
    /// mounts the tree; its own message says what is missing or stale.
    fn ensure_install(&self, up: &UpOpts) -> Result<()> {
        let exe = match &up.install_exe {
            Some(exe) => exe.clone(),
            None => std::env::current_exe().context("locating this binary for `install`")?,
        };
        let status = Command::new(&exe)
            .args(["install", "--platform", "wine", "ensure"])
            .env("CS2_MCP_PROJECT_ROOT", &self.paths.project)
            .env("CS2_GAME", self.paths.app.join("game"))
            .env("XDG_RUNTIME_DIR", &self.podman.xdg)
            .status()
            .with_context(|| format!("running {} install", exe.display()))?;
        if !status.success() {
            let rc = status
                .code()
                .map_or_else(|| "a signal".to_string(), |c| c.to_string());
            bail!(
                "install ensure failed (rc {rc}) - not starting; fix the artefacts above \
                 (cargo bridge install --help)"
            );
        }
        Ok(())
    }

    fn up(&self, up: &UpOpts) -> Result<()> {
        let tool = Tool::parse(&or_default(&up.tool, "hammer"))?;
        let wait_channel = seconds(&up.wait_channel_s, 180, "WAIT_CHANNEL_S")?;
        let wait_discovery = seconds(&up.wait_discovery_s, 90, "WAIT_DISCOVERY_S")?;
        let image = [&up.image, &up.cs2_mcp_image]
            .into_iter()
            .flatten()
            .find(|i| !i.is_empty())
            .map_or(DEFAULT_IMAGE, String::as_str);
        let addon = (!up.addon.is_empty()).then_some(up.addon.as_str());
        let extra_args = match podman::with_insecure(&up.extra_args) {
            Some(extra) => {
                say("added -insecure to EXTRA_ARGS (Metamod:Source refuses without it)");
                extra
            }
            None => up.extra_args.clone(),
        };
        let env: Vec<(&'static str, String)> = [
            ("CS2MCP_TESTPLUGIN_MODE", &up.testplugin_mode),
            ("CS2MCP_TESTPLUGIN_LOG", &up.testplugin_log),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            value
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(|v| (name, v.clone()))
        })
        .collect();

        let lock = lock_file(&self.paths.modeldoc_lock, LOCK_WAIT)?;
        if self.is_modeldoc() {
            bail!(
                "{c} is the running ModelDoc instance; stop it explicitly first: cargo bridge runtime down --container {c}",
                c = self.container
            );
        }
        if tool == Tool::Modeldoc {
            self.guard_modeldoc()?;
        }
        self.down();
        // the tree is free of OUR container now: make sure the install is current
        say("checking the bridge install (install --platform wine ensure)...");
        self.ensure_install(up)?;
        self.clear_pet_crash_flags()?;
        match std::fs::remove_file(&self.paths.disc) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", self.paths.disc.display()));
            }
            _ => {}
        }
        // The Rust log is appended to across sessions, and the Metamod copy writes to it during
        // engine start, so rotate it before the launch: the Init wait must only see this
        // session's lines.
        if self.paths.rs_log.exists() {
            let prev = self.paths.rs_log.with_extension("log.prev");
            std::fs::rename(&self.paths.rs_log, &prev)
                .with_context(|| format!("rotating {}", self.paths.rs_log.display()))?;
        }
        let xauth = xauth_file()
            .filter(|x| x.is_file())
            .context("no XAUTHORITY file found (set XAUTHORITY)")?;
        say(format!(
            "launching {} (tool={}, addon='{}', xauth={})",
            self.container,
            tool.as_str(),
            addon.unwrap_or("none"),
            xauth.display()
        ));
        let argv = podman::run_argv(&Launch {
            container: &self.container,
            image,
            tool,
            addon,
            extra_args: &extra_args,
            env: &env,
            app: &self.paths.app,
            cache: &self.paths.cache,
            xauth: &xauth,
        });
        let status = self
            .podman
            .command(&argv)
            .stdout(Stdio::null())
            .status()
            .context("running podman run")?;
        if !status.success() {
            bail!("podman run failed ({status})");
        }
        // the label is visible to `podman ps` from here on
        drop(lock);
        say("waiting for the VToolMessage channel (engine start)...");
        self.wait_channel(wait_channel)?;
        say("loading cs2_bridge...");
        self.load_bridge(wait_discovery)
    }

    /// The VToolMessage mapping exists once toolframework2 Init ran.
    fn wait_channel(&self, wait: Duration) -> Result<()> {
        self.put_payload(&cs2_vtm::command("echo mcp_runtime_channel_probe"))?;
        let start = Instant::now();
        let up = poll(start + wait, VTM_POLL, || {
            self.vtmsend()
                .ok()
                .filter(|(_, out)| out.contains(CHANNEL_UP))
                .map(|_| ())
        });
        if up.is_none() {
            bail!(
                "VToolMessage channel never appeared ({}s); the engine may not have started (needs the focus \
                 flags: +engine_no_focus_sleep 0 +r_always_render_all_windows 1) - see: podman logs {}",
                wait.as_secs(),
                self.container
            );
        }
        say(format!("  channel up after {}s", start.elapsed().as_secs()));
        Ok(())
    }

    /// The bridge load: a vtm `tool_request` for `cs2_bridge`, re-posted until the tool role's
    /// Init line ([`RS_INIT_RE`]) shows in its log, then the discovery file the listener writes
    /// on its first tick. Each wait gets `wait`.
    fn load_bridge(&self, wait: Duration) -> Result<()> {
        self.put_payload(&cs2_vtm::bridge_request())?;
        let init = regex::Regex::new(RS_INIT_RE).expect("valid regex");
        let start = Instant::now();
        let mut posted: Option<Instant> = None;
        let found = poll(start + wait, FILE_POLL, || {
            if std::fs::read_to_string(&self.paths.rs_log).is_ok_and(|log| init.is_match(&log)) {
                return Some(());
            }
            if posted.is_none_or(|t| t.elapsed() >= BRIDGE_REPOST) {
                let _ = self.vtmsend();
                posted = Some(Instant::now());
            }
            None
        });
        if found.is_none() {
            bail!(
                "rust bridge never logged its tool-role Init (matching '{RS_INIT_RE}') after {}s - check {}",
                wait.as_secs(),
                self.paths.rs_log.display()
            );
        }
        say(format!(
            "  rust bridge Init after {}s",
            start.elapsed().as_secs()
        ));
        let start = Instant::now();
        if poll(start + wait, FILE_POLL, || {
            self.paths.disc.is_file().then_some(())
        })
        .is_none()
        {
            bail!(
                "no discovery file after {}s - the Rust listener did not start (Init ran, so check {} for \
                 'bridge: starting listener' / 'ipc: listening')",
                wait.as_secs(),
                self.paths.rs_log.display()
            );
        }
        say(format!(
            "  discovery file after {}s",
            start.elapsed().as_secs()
        ));
        Ok(())
    }

    fn open(&self, asset: Option<&str>, open: &OpenOpts) -> Result<u8> {
        let poc = or_default(&open.poc_asset, DEFAULT_POC_ASSET);
        let asset = asset.filter(|a| !a.is_empty()).unwrap_or(&poc);
        let wait_doc = seconds(&open.wait_doc_s, 60, "WAIT_DOC_S")?;
        let modeldoc = self.is_modeldoc();
        if (asset.ends_with(".vmdl") || asset.ends_with(".vmdl_c")) && !modeldoc {
            bail!(
                "{asset} is a model: open it in the ModelDoc instance (cargo bridge runtime up --tool modeldoc), \
                 not in {}",
                self.container
            );
        }
        self.put_payload(&cs2_vtm::open_asset(asset))?;
        say(format!("open_asset {asset}"));
        let (ok, out) = self.vtmsend()?;
        if !ok {
            bail!("vtmsend failed: {}", out.trim());
        }
        let (method, tool_dll) = if modeldoc {
            ("modeldoc.active", "modeldoc_editor.dll")
        } else {
            ("doc.active", "hammer.dll")
        };
        if !self.wait_doc(method, wait_doc) {
            return Ok(1);
        }
        Ok(if self.wait_modules(tool_dll) { 0 } else { 1 })
    }

    /// Polls the tool's active-document method until a document is open.
    fn wait_doc(&self, method: &str, wait: Duration) -> bool {
        let start = Instant::now();
        let deadline = start + wait;
        let doc = poll(deadline, VTM_POLL, || {
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .clamp(Duration::from_secs(1), Duration::from_secs(30));
            let disc = rpc::read_discovery(&self.paths.disc).ok()?;
            let mut session = rpc::Session::connect(&disc, timeout).ok()?;
            rpc::open_doc(&session.call("d", method, json!({})).ok()?)
        });
        match doc {
            Some(_) => {
                say(format!(
                    "  document open after {}s",
                    start.elapsed().as_secs()
                ));
                true
            }
            None => {
                say(format!(
                    "warn: no document after {}s (a modal may be up; screenshot to check)",
                    wait.as_secs()
                ));
                false
            }
        }
    }

    /// Waits until the module identity checks reach a terminal state.
    fn wait_modules(&self, tool_dll: &str) -> bool {
        let done = poll(Instant::now() + WAIT_MODULES, VTM_POLL, || {
            let disc = rpc::read_discovery(&self.paths.disc).ok()?;
            let mut session = rpc::Session::connect(&disc, Duration::from_secs(10)).ok()?;
            let reply = session.call("m", "control.capabilities", json!({})).ok()?;
            match rpc::modules_progress(&reply, tool_dll).ok()? {
                rpc::Modules::Done(states) => Some(states),
                rpc::Modules::Pending(_) => None,
            }
        });
        match done {
            Some(states) => {
                say(format!("  modules: {states}"));
                true
            }
            None => {
                say(format!(
                    "warn: module verification did not settle in {}s",
                    WAIT_MODULES.as_secs()
                ));
                false
            }
        }
    }

    /// The container's state, the discovery file, and whether the bridge answers `control.hello`
    /// (the script ran test_client.py here; that live suite is gone, the handshake is the
    /// liveness proof that is left).
    fn status(&self) -> Result<()> {
        let state = match self.podman.output(&podman::ps_status_argv(&self.container)) {
            Ok(out) => {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
                String::from_utf8_lossy(&out.stdout).trim_end().to_string()
            }
            Err(e) => {
                say(format!("podman: {e}"));
                String::new()
            }
        };
        say(format!("container: {state}"));
        let Ok(text) = std::fs::read_to_string(&self.paths.disc) else {
            say("discovery: absent");
            return Ok(());
        };
        say(format!("discovery: {}", text.replace(['\n', ' '], "")));
        let hello = rpc::read_discovery(&self.paths.disc)
            .and_then(|d| rpc::Session::connect(&d, Duration::from_secs(5)));
        match hello {
            Ok(session) => match session.hello.get("result") {
                Some(result) => say(format!(
                    "bridge: alive (bridgeVersion {}, gameBuild {})",
                    result
                        .get("bridgeVersion")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    result
                        .get("gameBuild")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                )),
                None => say(format!(
                    "bridge: hello refused: {}",
                    session.hello.get("error").unwrap_or(&session.hello)
                )),
            },
            Err(e) => say(format!("bridge: not reachable ({e:#})")),
        }
        Ok(())
    }

    fn logs(&self, n: usize) {
        for (title, path, missing) in [
            ("bridge log", &self.paths.rs_log, "(no bridge log)"),
            ("game console", &self.paths.console_log, "(no console log)"),
        ] {
            say(format!("=== {title} (last {n}) ==="));
            match std::fs::read(path) {
                Ok(bytes) => print!("{}", tail(&String::from_utf8_lossy(&bytes), n)),
                Err(_) => say(missing),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pet_flags() {
        let text =
            "\"Closed Correctly\" = false\n\"ClosedCorrectly 2\"\t=\tfalse\n\"Other\" = false\n";
        assert_eq!(
            clear_closed_flags(text).unwrap(),
            "\"Closed Correctly\" = true\n\"ClosedCorrectly 2\"\t=\ttrue\n\"Other\" = false\n"
        );
        assert_eq!(clear_closed_flags("\"Closed Correctly\" = true\n"), None);
        // the value is on the flag's line
        assert_eq!(clear_closed_flags("\"Closed Correctly\" =\nfalse\n"), None);
    }

    #[test]
    fn tail_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
        assert_eq!(tail("a\nb\n", 5), "a\nb\n");
        assert_eq!(tail("a\nb\n", 0), "");
        assert_eq!(tail("", 3), "");
    }

    #[test]
    fn empty_means_default() {
        assert_eq!(or_default("", "cs2tools"), "cs2tools");
        assert_eq!(or_default("x", "cs2tools"), "x");
        assert_eq!(seconds("", 90, "W").unwrap(), Duration::from_secs(90));
        assert_eq!(seconds("5", 90, "W").unwrap(), Duration::from_secs(5));
        assert!(seconds("soon", 90, "W").is_err());
    }

    #[test]
    fn init_line() {
        let re = regex::Regex::new(RS_INIT_RE).unwrap();
        assert!(re.is_match("x [cs2-bridge mm+tool] Init ok"));
        assert!(re.is_match("[cs2-bridge tool] Init"));
        assert!(!re.is_match("[cs2-bridge mm] Init"));
    }

    /// The lock interlocks across open file descriptions (two `up`s are two processes; two
    /// opens in one process conflict the same way), and a second taker gives up at its deadline.
    #[test]
    fn modeldoc_lock_excludes() {
        // under target/, not /tmp (AGENTS.md rule 6)
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let path = dir.path().join(".podman/cs2mcp-modeldoc.lock");
        let held = lock_file(&path, Duration::ZERO).unwrap();
        let start = Instant::now();
        let err = lock_file(&path, Duration::from_millis(300)).unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert!(
            err.to_string().contains("another `runtime up` holds it"),
            "{err}"
        );
        drop(held);
        lock_file(&path, Duration::ZERO).unwrap();
    }
}
