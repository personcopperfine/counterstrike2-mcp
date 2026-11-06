//! The window: settings, status, the actions, the readiness ladder, the log, and the dialogs for
//! findings that need the user's decision (never resolved behind their back).

use crate::register::{self, Line};
use crate::worker::{self, Action, Job, Outcome, Settings};
use cs2_native::events::Event;
use cs2_native::findings::{Code, Finding, Policy, Severity};
use cs2_native::install::Os;
use cs2_native::mcpconfig::Transport;
use cs2_native::plan::SteamMode;
use cs2_native::state::Strategy;
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// The readiness ladder, in order (`session::wait_ready`).
const STAGES: [(&str, &str); 6] = [
    ("ProcessUp", "game process up"),
    ("MetamodLoaded", "Metamod loaded"),
    ("ConsoleLogExists", "console log"),
    ("ToolChannel", "tool channel"),
    ("DiscoveryFresh", "bridge discovery file"),
    ("BridgeHello", "bridge answers"),
];

const LOG_MAX: usize = 4000;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 760.0]).with_title("cs2-mcp launcher"),
        ..Default::default()
    };
    eframe::run_native("cs2mcp-launcher", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

enum Dialog {
    /// findings of an action: stopped before it changed anything, or reported after it finished
    Findings { action: Action, policy: Policy, title: String, findings: Vec<Finding>, stopped: bool },
    /// what a cleanup would move out of the install
    ConfirmCleanup { steps: Vec<String> },
    /// declare the missing backups lost
    ConfirmLost { action: Action, policy: Policy, paths: Vec<String> },
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Desktop,
    Code,
    Copy,
}

struct RegisterUi {
    tab: Tab,
    candidates: Vec<(PathBuf, bool)>,
    path: String,
    preview: Option<Result<(String, Vec<Line>), String>>,
    written: Option<String>,
    replace: bool,
    code_run: Option<Receiver<String>>,
    code_log: String,
}

impl RegisterUi {
    fn new() -> RegisterUi {
        let candidates = register::desktop_candidates();
        let path = candidates.first().map(|(p, _)| p.display().to_string()).unwrap_or_default();
        RegisterUi { tab: Tab::Desktop, candidates, path, preview: None, written: None, replace: true, code_run: None, code_log: String::new() }
    }
}

pub struct App {
    settings: Settings,
    game_root: String,
    library: String,
    state_dir: String,
    strategy: Strategy,
    job: Option<Job>,
    queued: Vec<(Action, Policy)>,
    log: Vec<Event>,
    stages: Vec<(String, f64)>,
    status: Option<Value>,
    last: Option<(Action, Value)>,
    error: Option<String>,
    dialog: Option<Dialog>,
    register: Option<RegisterUi>,
    mcp: Option<Value>,
    mcp_notes: Vec<String>,
    transport: Transport,
}

fn severity_color(s: Severity) -> Color32 {
    match s {
        Severity::Info => Color32::GRAY,
        Severity::Warn => Color32::from_rgb(220, 170, 40),
        Severity::Decision => Color32::from_rgb(230, 130, 30),
        Severity::Blocker => Color32::from_rgb(220, 60, 60),
    }
}

fn findings_of(v: &Value) -> Vec<Finding> {
    serde_json::from_value(v["findings"].clone()).unwrap_or_default()
}

fn opt_path(s: &str) -> Option<PathBuf> {
    let t = s.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        let mut a = App {
            settings: Settings::default(),
            game_root: std::env::var("CS2MCP_GAME_ROOT").unwrap_or_default(),
            library: String::new(),
            state_dir: String::new(),
            strategy: Strategy::Auto,
            job: None,
            queued: Vec::new(),
            log: Vec::new(),
            stages: Vec::new(),
            status: None,
            last: None,
            error: None,
            dialog: None,
            register: None,
            mcp: None,
            mcp_notes: Vec::new(),
            transport: Transport::Stdio,
        };
        a.start(ctx, Action::Status, Policy::default());
        a
    }

    fn current_settings(&self) -> Settings {
        let mut s = self.settings.clone();
        s.ctx.game_root = opt_path(&self.game_root);
        s.ctx.library = opt_path(&self.library).into_iter().collect();
        s.ctx.state_dir = opt_path(&self.state_dir);
        s.ctx.strategy = Some(self.strategy);
        s
    }

    fn start(&mut self, ctx: &egui::Context, action: Action, policy: Policy) {
        if self.job.is_some() {
            self.queued.push((action, policy));
            return;
        }
        self.error = None;
        if action == Action::Up {
            self.stages.clear();
        }
        let c = ctx.clone();
        self.job = Some(worker::start(action, &self.current_settings(), policy, move || c.request_repaint()));
    }

    /// Take what the worker sent since the last frame.
    fn poll(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        while let Ok(e) = job.events.try_recv() {
            if let Event::Stage { stage, seconds } = &e {
                self.stages.push((stage.clone(), *seconds));
            }
            self.log.push(e);
        }
        if self.log.len() > LOG_MAX {
            self.log.drain(..self.log.len() - LOG_MAX);
        }
        let Ok(out) = job.done.try_recv() else { return };
        let (action, policy) = (job.action, job.policy);
        self.job = None;
        match out {
            Outcome::Done(v) => self.finished(action, policy, v),
            Outcome::Stopped(st) => {
                self.dialog = Some(Dialog::Findings { action, policy, title: st.message.clone(), findings: st.findings, stopped: true });
            }
            Outcome::Failed(e) => self.error = Some(format!("{}: {e}", action.label())),
        }
        let changed = matches!(action, Action::Prepare | Action::Up | Action::Stop | Action::Uninstall | Action::Cleanup);
        if changed && !self.queued.iter().any(|(a, _)| *a == Action::Status) {
            self.queued.push((Action::Status, Policy::default()));
        }
        if !self.queued.is_empty() {
            let (a, p) = self.queued.remove(0);
            self.start(ctx, a, p);
        }
    }

    fn finished(&mut self, action: Action, policy: Policy, v: Value) {
        match action {
            Action::Status => self.status = Some(v),
            Action::McpConfig => {
                self.mcp = Some(v["config"]["mcpServers"].clone());
                self.mcp_notes = serde_json::from_value(v["notes"].clone()).unwrap_or_default();
            }
            Action::CleanupPreview => {
                let steps: Vec<String> = serde_json::from_value(v["wouldDo"].clone()).unwrap_or_default();
                if steps.is_empty() {
                    self.log.push(Event::Info { text: "nothing to clean up".into() });
                } else {
                    self.dialog = Some(Dialog::ConfirmCleanup { steps });
                }
            }
            _ => {
                let f = findings_of(&v);
                if f.iter().any(|f| f.severity >= Severity::Warn) {
                    let title = format!("{} finished; look at these", action.label());
                    self.dialog = Some(Dialog::Findings { action, policy, title, findings: f, stopped: false });
                }
                self.last = Some((action, v));
            }
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn top(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            ui.heading("cs2-mcp");
            ui.label(RichText::new(format!("launcher {}", env!("CARGO_PKG_VERSION"))).weak());
            if let Some(j) = &self.job {
                ui.spinner();
                ui.label(format!("{} … {:.0}s", j.action.label(), j.started.elapsed().as_secs_f64()));
            }
        });
        ui.horizontal_wrapped(|ui| {
            let idle = !self.busy();
            if ui.add_enabled(idle, egui::Button::new("Refresh status")).clicked() {
                self.start(&ctx, Action::Status, Policy::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Prepare + Launch")).on_hover_text("lay the files down (or stage the overlay), start the game, wait until the bridge answers").clicked() {
                self.start(&ctx, Action::Up, Policy::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Prepare only")).clicked() {
                self.start(&ctx, Action::Prepare, Policy::default());
            }
            let waiting = self.job.as_ref().is_some_and(|j| j.action == Action::Up);
            if ui.add_enabled(idle || waiting, egui::Button::new("Stop")).on_hover_text("stop the game, restore the install, verify it").clicked() {
                if let Some(j) = &self.job {
                    j.cancel();
                }
                self.start(&ctx, Action::Stop, Policy::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Repair")).on_hover_text("restore a prepared install without a session (after a crash or a reboot)").clicked() {
                self.start(&ctx, Action::Uninstall, Policy::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Clean up leftovers…")).on_hover_text("files of ours no manifest records (a deleted state dir, an older install-rs.sh layout)").clicked() {
                self.start(&ctx, Action::CleanupPreview, Policy::default());
            }
            if ui.button("Register with Claude…").clicked() {
                self.register = Some(RegisterUi::new());
                self.start(&ctx, Action::McpConfig, Policy::default());
            }
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Session").default_open(true).show(ui, |ui| {
            egui::Grid::new("session").num_columns(2).show(ui, |ui| {
                ui.label("Steam");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.settings.steam, SteamMode::Gbe, "gbe emulator (offline, no account)");
                    ui.radio_value(&mut self.settings.steam, SteamMode::Client, "my Steam client");
                });
                ui.end_row();
                ui.label("Workshop Tools");
                ui.add_enabled(Os::host() == Os::Windows, egui::Checkbox::new(&mut self.settings.tools, "-tools (Hammer, ModelDoc)")).on_disabled_hover_text("Windows only");
                ui.end_row();
                ui.label("Map");
                ui.add(egui::TextEdit::singleline(&mut self.settings.map).hint_text("de_dust2 (client sessions start the bridge once a map runs)"));
                ui.end_row();
                ui.label("Addon");
                ui.add(egui::TextEdit::singleline(&mut self.settings.addon).hint_text("-addon (tools)"));
                ui.end_row();
                ui.label("Extra game args");
                ui.add(egui::TextEdit::singleline(&mut self.settings.extra).hint_text("-insecure is always added; -secure is refused"));
                ui.end_row();
                ui.label("");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.settings.capture, "capture layer (screenshot/observe)");
                    ui.checkbox(&mut self.settings.keep_installed, "keep the files after the game exits");
                });
                ui.end_row();
            });
            if self.settings.steam == SteamMode::Client {
                ui.colored_label(
                    severity_color(Severity::Warn),
                    "With your Steam client the game always runs with -insecure: VAC off, no official or secure servers (it carries injected code).",
                );
            }
        });
        egui::CollapsingHeader::new("Install and state").default_open(false).show(ui, |ui| {
            egui::Grid::new("paths").num_columns(2).show(ui, |ui| {
                ui.label("Game root");
                ui.add(egui::TextEdit::singleline(&mut self.game_root).hint_text("found in Steam's libraries").desired_width(520.0));
                ui.end_row();
                ui.label("Extra Steam library");
                ui.add(egui::TextEdit::singleline(&mut self.library).hint_text("e.g. D:\\SteamLibrary").desired_width(520.0));
                ui.end_row();
                ui.label("State dir");
                ui.add(egui::TextEdit::singleline(&mut self.state_dir).hint_text("default: %LOCALAPPDATA% / $XDG_STATE_HOME").desired_width(520.0));
                ui.end_row();
                ui.label("Strategy");
                egui::ComboBox::from_id_salt("strategy").selected_text(format!("{:?}", self.strategy)).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.strategy, Strategy::Auto, "Auto");
                    if Os::host() == Os::Linux {
                        ui.selectable_value(&mut self.strategy, Strategy::Overlay, "Overlay (the install is never written)");
                    }
                    ui.selectable_value(&mut self.strategy, Strategy::InPlace, "InPlace (manifest + backups, restored on Stop)");
                });
                ui.end_row();
            });
        });
    }

    fn findings_ui(ui: &mut egui::Ui, f: &[Finding]) {
        for f in f {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(severity_color(f.severity), RichText::new(format!("{:?}", f.severity).to_lowercase()).strong());
                ui.label(&f.title);
            });
            ui.indent(("f", &f.title), |ui| {
                for p in &f.paths {
                    ui.monospace(p);
                }
                for a in &f.advice {
                    ui.label(RichText::new(format!("→ {a}")).italics());
                }
            });
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Status");
        let Some(s) = &self.status else {
            ui.label("…");
            return;
        };
        let text = |v: &Value| match v {
            Value::Null => "—".to_string(),
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        egui::Grid::new("status").num_columns(2).striped(true).show(ui, |ui| {
            let i = &s["install"];
            ui.label("Install");
            ui.monospace(text(&i["root"]));
            ui.end_row();
            ui.label("Game build");
            let (b, t) = (text(&i["build"]), text(&i["targetBuild"]));
            if b == t {
                ui.label(format!("{b} (the build this release targets)"));
            } else {
                ui.colored_label(severity_color(Severity::Warn), format!("{b}; this release targets {t}"));
            }
            ui.end_row();
            ui.label("Strategy");
            ui.label(text(&s["strategy"]));
            ui.end_row();
            ui.label("Prepared");
            match s["manifest"].as_object() {
                Some(m) => ui.label(format!("{} ({} entries, steam {})", text(&m["state"]), text(&m["entries"]), text(&m["steam"]))),
                None => ui.label("no (the install is as Steam left it)"),
            };
            ui.end_row();
            ui.label("Session");
            match s["session"].as_object() {
                Some(x) => ui.label(format!(
                    "{} {}{}",
                    text(&x["phase"]),
                    if x["alive"] == true { "(running)" } else { "(not running)" },
                    x.get("error").filter(|e| !e.is_null()).map(|e| format!(": {}", text(e))).unwrap_or_default()
                )),
                None => ui.label("none"),
            };
            ui.end_row();
            if !s["lockHeldBy"].is_null() {
                ui.label("Busy");
                ui.colored_label(severity_color(Severity::Warn), text(&s["lockHeldBy"]));
                ui.end_row();
            }
            ui.label("State dir");
            ui.monospace(text(&s["stateDir"]));
            ui.end_row();
        });
        if let Some(d) = s["manifest"]["drift"].as_array().filter(|d| !d.is_empty()) {
            ui.label(format!("{} file(s) changed since we wrote them:", d.len()));
        }
        let f = findings_of(s);
        if !f.is_empty() {
            ui.separator();
            Self::findings_ui(ui, &f);
        }
    }

    fn ladder_ui(&self, ui: &mut egui::Ui) {
        let running = self.job.as_ref().is_some_and(|j| j.action == Action::Up);
        if self.stages.is_empty() && !running {
            return;
        }
        ui.separator();
        ui.heading("Readiness");
        for (key, label) in STAGES {
            match self.stages.iter().find(|(s, _)| s == key) {
                Some((_, secs)) => ui.label(format!("✔ {label}  ({secs:.1}s)")),
                None => ui.label(RichText::new(format!("○ {label}")).weak()),
            };
        }
    }

    fn log_ui(&self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for e in &self.log {
                match e {
                    Event::Warn { text } => ui.colored_label(severity_color(Severity::Warn), text),
                    Event::Stage { .. } => ui.colored_label(Color32::from_rgb(90, 180, 90), e.line()),
                    Event::Info { text } => ui.monospace(text),
                };
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(d) = &self.dialog else { return };
        let mut close = false;
        let mut next: Option<(Action, Policy)> = None;
        let mut then: Option<Dialog> = None;
        egui::Modal::new(egui::Id::unique("cs2mcp-dialog")).show(ctx, |ui| {
            ui.set_max_width(760.0);
            match d {
                Dialog::Findings { action, policy, title, findings, stopped } => {
                    ui.heading(if *stopped { "A decision is needed" } else { "Done, with notes" });
                    ui.label(title);
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| Self::findings_ui(ui, findings));
                    ui.separator();
                    let blocker = findings.iter().any(|f| f.severity == Severity::Blocker && f.code != Code::MissingBackups);
                    let decision = findings.iter().any(|f| f.severity == Severity::Decision);
                    let has = |c: Code| findings.iter().any(|f| f.code == c);
                    ui.horizontal_wrapped(|ui| {
                        if *stopped && decision && !blocker && ui.button("Continue anyway").on_hover_text("keep what was found as it is (--accept-conflicts)").clicked() {
                            next = Some((*action, Policy { accept_conflicts: true, ..*policy }));
                            close = true;
                        }
                        if (has(Code::Orphans) || has(Code::Legacy) || has(Code::ReplacedGameFile)) && ui.button("Clean up…").clicked() {
                            next = Some((Action::CleanupPreview, Policy::default()));
                            close = true;
                        }
                        if has(Code::MissingBackups) && ui.button("Mark as lost…").clicked() {
                            let paths = findings.iter().filter(|f| f.code == Code::MissingBackups).flat_map(|f| f.paths.clone()).collect();
                            then = Some(Dialog::ConfirmLost { action: *action, policy: *policy, paths });
                        }
                        if ui.button("Copy details").clicked() {
                            ui.ctx().copy_text(serde_json::to_string_pretty(findings).unwrap_or_default());
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::ConfirmCleanup { steps } => {
                    ui.heading("Clean up the install?");
                    ui.label("These are moved out of the install into the state dir (cleanup-<time>/); nothing is deleted, gameinfo.gi is never edited:");
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                        for s in steps {
                            ui.monospace(s);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Move them out").clicked() {
                            next = Some((Action::Cleanup, Policy::default()));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::ConfirmLost { action, policy, paths } => {
                    ui.heading("Mark the missing backups as lost?");
                    ui.label("The launcher's copies of these game files are gone. Everything else is restored; our bytes are removed from these, and Valve's own come back only through Steam:");
                    for p in paths {
                        ui.monospace(p);
                    }
                    ui.label(RichText::new(cs2_native::findings::VERIFY_ADVICE).italics());
                    ui.horizontal(|ui| {
                        if ui.button("Mark as lost and restore the rest").clicked() {
                            next = Some((*action, Policy { forget_missing_backups: true, ..*policy }));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if let Some(t) = then {
            self.dialog = Some(t);
        } else if close {
            self.dialog = None;
        }
        if let Some((a, p)) = next {
            self.start(ctx, a, p);
        }
    }

    fn register_window(&mut self, ctx: &egui::Context) {
        let Some(r) = &mut self.register else { return };
        if let Some(rx) = &r.code_run {
            while let Ok(s) = rx.try_recv() {
                r.code_log.push_str(&s);
            }
        }
        let mut open = true;
        let servers = self.mcp.clone();
        let notes = self.mcp_notes.clone();
        let transport = &mut self.transport;
        egui::Window::new("Register with an MCP client").open(&mut open).default_width(760.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Transport");
                egui::ComboBox::from_id_salt("transport").selected_text(transport.label()).show_ui(ui, |ui| {
                    ui.selectable_value(transport, Transport::Stdio, Transport::Stdio.label());
                    ui.add_enabled_ui(Transport::Http.available(), |ui| {
                        ui.selectable_value(transport, Transport::Http, Transport::Http.label());
                    });
                });
            });
            ui.label("stdio servers are started by your MCP client (Claude Desktop, Claude Code) from this config; the launcher does not run them. Start the game here first: the servers attach to its session.");
            for n in &notes {
                ui.label(RichText::new(format!("note: {n}")).weak());
            }
            ui.separator();
            let Some(servers) = servers else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("reading the install…");
                });
                return;
            };
            ui.horizontal(|ui| {
                ui.selectable_value(&mut r.tab, Tab::Desktop, "Claude Desktop");
                ui.selectable_value(&mut r.tab, Tab::Code, "Claude Code");
                ui.selectable_value(&mut r.tab, Tab::Copy, "Copy JSON");
            });
            ui.separator();
            match r.tab {
                Tab::Desktop => {
                    ui.label("Config file (claude_desktop_config.json):");
                    for (p, exists) in &r.candidates {
                        let s = p.display().to_string();
                        if ui.radio(r.path == s, format!("{s}{}", if *exists { "" } else { "  (new)" })).clicked() {
                            r.path = s;
                            r.preview = None;
                        }
                    }
                    if r.candidates.is_empty() {
                        ui.label(RichText::new("No Claude Desktop config found here (there is no official Linux build). Type the file's path if you have one, or copy the JSON.").weak());
                    }
                    if ui.add(egui::TextEdit::singleline(&mut r.path).desired_width(700.0)).changed() {
                        r.preview = None;
                    }
                    if ui.add_enabled(!r.path.trim().is_empty(), egui::Button::new("Show the changes")).clicked() {
                        let p = PathBuf::from(r.path.trim());
                        let old = std::fs::read_to_string(&p).ok();
                        r.written = None;
                        r.preview = Some(register::merge(old.as_deref(), &servers).map(|new| {
                            let d = register::diff(old.as_deref().unwrap_or(""), &new);
                            (new, d)
                        }).map_err(|e| format!("{e:#}")));
                    }
                    match &r.preview {
                        Some(Ok((new, d))) => {
                            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                                for l in d {
                                    match l {
                                        Line::Same(s) => ui.monospace(RichText::new(format!("  {s}")).weak()),
                                        Line::Removed(s) => ui.monospace(RichText::new(format!("- {s}")).color(Color32::from_rgb(220, 90, 90))),
                                        Line::Added(s) => ui.monospace(RichText::new(format!("+ {s}")).color(Color32::from_rgb(90, 190, 90))),
                                    };
                                }
                            });
                            let changed = d.iter().any(|l| !matches!(l, Line::Same(_)));
                            if !changed {
                                ui.label("the file already has these entries");
                            } else if ui.button("Back up the file and write").clicked() {
                                let p = PathBuf::from(r.path.trim());
                                r.written = Some(match register::write(&p, new) {
                                    Ok(Some(b)) => format!("written; the old file is at {}. Restart Claude Desktop to load it.", b.display()),
                                    Ok(None) => "written (a new file). Restart Claude Desktop to load it.".to_string(),
                                    Err(e) => format!("not written: {e:#}"),
                                });
                                r.preview = None;
                            }
                        }
                        Some(Err(e)) => {
                            ui.colored_label(severity_color(Severity::Blocker), e);
                        }
                        None => {}
                    }
                    if let Some(w) = &r.written {
                        ui.label(w);
                    }
                }
                Tab::Code => {
                    ui.checkbox(&mut r.replace, "replace entries of the same name");
                    let cmds = register::claude_code_commands(&servers, r.replace);
                    let lines: Vec<String> = cmds.iter().map(|c| register::shell_line("claude", c)).collect();
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for l in &lines {
                            ui.monospace(l);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Copy the commands").clicked() {
                            ui.ctx().copy_text(lines.join("\n"));
                        }
                        match register::claude_cli() {
                            Some(cli) => {
                                let running = r.code_run.is_some() && r.code_log.ends_with("…\n");
                                if ui.add_enabled(!running, egui::Button::new(format!("Run them ({})", cli.display()))).clicked() {
                                    r.code_log.clear();
                                    r.code_run = Some(run_claude(cli, cmds, ui.ctx().clone()));
                                }
                            }
                            None => {
                                ui.label(RichText::new("`claude` is not on PATH: copy the commands into a terminal").weak());
                            }
                        }
                    });
                    if !r.code_log.is_empty() {
                        egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| ui.monospace(&r.code_log));
                    }
                }
                Tab::Copy => {
                    let text = serde_json::to_string_pretty(&serde_json::json!({"mcpServers": servers})).unwrap_or_default();
                    if ui.button("Copy to clipboard").clicked() {
                        ui.ctx().copy_text(text.clone());
                    }
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| ui.monospace(text));
                }
            }
        });
        if !open {
            self.register = None;
        }
    }
}

/// Run the `claude mcp` commands one after the other; their output comes back line by line.
fn run_claude(cli: PathBuf, cmds: Vec<Vec<String>>, ctx: egui::Context) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for c in cmds {
            let _ = tx.send(format!("$ {}\n", register::shell_line("claude", &c)));
            let mut cmd = std::process::Command::new(&cli);
            cmd.args(&c);
            cs2_native::selfexe::scrub_appimage_env(&mut cmd);
            let out = match cmd.output() {
                Ok(o) => format!("{}{}(exit {})\n", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr), o.status.code().unwrap_or(-1)),
                Err(e) => format!("{e}\n"),
            };
            let _ = tx.send(out);
            ctx.request_repaint();
        }
        let _ = tx.send("done\n".into());
        ctx.request_repaint();
    });
    rx
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        egui::Panel::top("top").show(ui, |ui| self.top(ui));
        egui::Panel::bottom("log").resizable(true).default_size(220.0).show(ui, |ui| self.log_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(e) = &self.error {
                    ui.colored_label(severity_color(Severity::Blocker), e);
                    ui.separator();
                }
                self.settings_ui(ui);
                ui.separator();
                self.status_ui(ui);
                self.ladder_ui(ui);
                if let Some((a, v)) = &self.last {
                    ui.separator();
                    egui::CollapsingHeader::new(format!("Last result: {}", a.label())).default_open(false).show(ui, |ui| {
                        ui.monospace(serde_json::to_string_pretty(v).unwrap_or_default());
                    });
                }
            });
        });
        self.dialogs(&ctx);
        self.register_window(&ctx);
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
