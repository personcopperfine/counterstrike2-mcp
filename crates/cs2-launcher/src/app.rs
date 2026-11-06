//! The window over the `cs2mcp-native` CLI (worker.rs runs it): settings, status, Prepare +
//! Launch with the readiness ladder, Stop (= `down`: stop, restore, verify), the log, and the
//! dialogs for findings that need the user's decision (never resolved behind their back). Each
//! answer is a CLI flag (`--accept-conflicts`, `--forget-missing-backups`) or command
//! (`cleanup --yes`).
//!
//! The window never leaves the install prepared because it was closed: closing while a session
//! is applied asks first, then runs `down` and closes only once the status says the install is
//! the user's again.

use crate::worker::{self, Action, Answers, Job, Outcome, Settings, Steam, Strategy};
use cs2_native::events::Event;
use cs2_native::findings::{Code, Finding, Severity};
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::collections::VecDeque;
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

/// Is the install prepared or a session alive, according to `status`?
pub fn applied(status: &Value) -> bool {
    status["manifest"].is_object() || status["session"]["alive"] == true
}

/// What a close request does: close now, or ask (and restore first).
#[derive(Debug, PartialEq, Eq)]
pub enum OnClose {
    Close,
    Ask,
}

/// `confirmed`: the restore verified; `busy`: a CLI command runs (or a close is restoring);
/// `status`: the last status read (`None`: not read yet).
pub fn on_close(confirmed: bool, busy: bool, status: Option<&Value>) -> OnClose {
    if confirmed {
        return OnClose::Close;
    }
    // unknown counts as applied: the first status read may still be running
    if busy || status.is_none_or(applied) {
        OnClose::Ask
    } else {
        OnClose::Close
    }
}

enum Dialog {
    /// findings of an action: stopped before it changed anything, or reported after it finished
    Findings { action: Action, answers: Answers, title: String, findings: Vec<Finding>, stopped: bool },
    /// what a cleanup would move out of the install; then the stopped action runs again
    ConfirmCleanup { steps: Vec<String> },
    /// declare the missing backups lost
    ConfirmLost { action: Action, answers: Answers, paths: Vec<String> },
    /// closing with a prepared install / a running game
    ConfirmClose,
    /// a command failed: the install is not as asked
    Error { title: String, text: String },
}

pub struct App {
    settings: Settings,
    job: Option<Job>,
    queued: VecDeque<(Action, Answers)>,
    log: Vec<Event>,
    stages: Vec<(String, f64)>,
    status: Option<Value>,
    last: Option<(Action, Value)>,
    dialog: Option<Dialog>,
    /// the action to run again after a cleanup the user chose from its findings
    resume: Option<(Action, Answers)>,
    /// the user asked to close: `down` runs, then the window closes once the status is clean
    closing: bool,
    /// the window may close now (the restore verified)
    close_ok: bool,
    /// Stop ended the readiness wait on purpose: its "ended" is not an error
    wait_ended: bool,
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

impl App {
    fn new(ctx: &egui::Context) -> App {
        let mut a = App {
            settings: Settings { game_root: std::env::var("CS2MCP_GAME_ROOT").unwrap_or_default(), ..Settings::default() },
            job: None,
            queued: VecDeque::new(),
            log: Vec::new(),
            stages: Vec::new(),
            status: None,
            last: None,
            dialog: None,
            resume: None,
            closing: false,
            close_ok: false,
            wait_ended: false,
        };
        a.start(ctx, Action::Status, Answers::default());
        a
    }

    fn start(&mut self, ctx: &egui::Context, action: Action, answers: Answers) {
        if self.job.is_some() {
            self.queued.push_back((action, answers));
            return;
        }
        if action == Action::Up {
            self.stages.clear();
        }
        let c = ctx.clone();
        self.job = Some(worker::start(action, &self.settings, answers, move || c.request_repaint()));
    }

    fn error(&mut self, title: impl Into<String>, text: impl Into<String>) {
        self.dialog = Some(Dialog::Error { title: title.into(), text: text.into() });
    }

    /// Take what the CLI sent since the last frame.
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
        let (action, answers) = (job.action, job.answers);
        self.job = None;
        match out {
            Outcome::Done(v) => self.finished(action, answers, v),
            Outcome::Stopped { message, findings } => {
                self.closing = false;
                self.resume = None;
                self.dialog = Some(Dialog::Findings { action, answers, title: message, findings, stopped: true });
            }
            Outcome::Failed(e) if action == Action::Wait && self.wait_ended => {
                self.log.push(Event::Info { text: format!("readiness wait ended (Stop): {e}") });
            }
            Outcome::Failed(e) => {
                let title = if self.closing {
                    "The window stays open: the install was not restored"
                } else {
                    match action {
                        Action::Prepare | Action::Up => "The game install was not prepared",
                        Action::Wait => "The game did not get ready",
                        Action::Stop | Action::Uninstall => "The install was not restored",
                        _ => "The command failed",
                    }
                };
                self.closing = false;
                self.resume = None;
                self.error(title, format!("{}: {e}", action.label()));
            }
        }
        if action == Action::Wait {
            self.wait_ended = false;
        }
        if action.changes() && !self.queued.iter().any(|(a, _)| *a == Action::Status) {
            self.queued.push_back((Action::Status, Answers::default()));
        }
        if let Some((a, p)) = self.queued.pop_front() {
            self.start(ctx, a, p);
        }
    }

    fn finished(&mut self, action: Action, answers: Answers, v: Value) {
        match action {
            Action::Status => {
                if self.closing && self.job.is_none() && self.queued.is_empty() {
                    if applied(&v) {
                        self.closing = false;
                        self.error("The window stays open", "the install still reads as prepared after Stop; look at the status, then Stop or Repair");
                    } else {
                        self.close_ok = true;
                    }
                }
                self.status = Some(v);
            }
            Action::Up => {
                // the ladder next, unless Stop is already queued
                if !self.queued.iter().any(|(a, _)| *a == Action::Stop) {
                    self.queued.push_front((Action::Wait, Answers::default()));
                }
                self.last = Some((action, v));
            }
            Action::CleanupPreview => {
                let steps: Vec<String> = serde_json::from_value(v["wouldDo"].clone()).unwrap_or_default();
                if steps.is_empty() {
                    self.log.push(Event::Info { text: "nothing to clean up".into() });
                    self.resume = None;
                } else {
                    self.dialog = Some(Dialog::ConfirmCleanup { steps });
                }
            }
            Action::Cleanup => {
                if let Some(r) = self.resume.take() {
                    self.queued.push_back(r);
                }
                self.last = Some((action, v));
            }
            _ => {
                let f = findings_of(&v);
                if f.iter().any(|f| f.severity >= Severity::Warn) {
                    // a restore with notes: shown; a close in progress waits for the user
                    self.closing = false;
                    let title = format!("{} finished; look at these", action.label());
                    self.dialog = Some(Dialog::Findings { action, answers, title, findings: f, stopped: false });
                }
                self.last = Some((action, v));
            }
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Stop = `down`. A running readiness wait is ended first (it changes nothing); a running
    /// prepare/launch finishes and `down` runs after it.
    fn stop(&mut self, ctx: &egui::Context) {
        if let Some(j) = &self.job {
            if j.action == Action::Wait {
                self.wait_ended = true;
                j.cancel();
            }
        }
        self.queued.retain(|(a, _)| *a != Action::Wait);
        if !self.queued.iter().any(|(a, _)| *a == Action::Stop) {
            self.start(ctx, Action::Stop, Answers::default());
        }
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
            if self.closing {
                ui.colored_label(severity_color(Severity::Warn), "restoring before the window closes…");
            }
        });
        ui.horizontal_wrapped(|ui| {
            let idle = !self.busy() && !self.closing;
            if ui.add_enabled(idle, egui::Button::new("Refresh status")).clicked() {
                self.start(&ctx, Action::Status, Answers::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Prepare + Launch")).on_hover_text("cs2mcp-native up: lay the files down (or stage the overlay), start the game, wait until the bridge answers").clicked() {
                self.start(&ctx, Action::Up, Answers::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Prepare only")).on_hover_text("cs2mcp-native prepare").clicked() {
                self.start(&ctx, Action::Prepare, Answers::default());
            }
            let running = self.job.as_ref().is_some_and(|j| matches!(j.action, Action::Up | Action::Wait | Action::Prepare));
            if ui.add_enabled((idle || running) && !self.closing, egui::Button::new("Stop")).on_hover_text("cs2mcp-native down: stop the game, restore the install, verify it").clicked() {
                self.stop(&ctx);
            }
            if ui.add_enabled(idle, egui::Button::new("Repair")).on_hover_text("cs2mcp-native uninstall: restore a prepared install without a session (after a crash or a reboot)").clicked() {
                self.start(&ctx, Action::Uninstall, Answers::default());
            }
            if ui.add_enabled(idle, egui::Button::new("Clean up leftovers…")).on_hover_text("cs2mcp-native cleanup: files of ours no manifest records (a deleted state dir, a `cargo bridge install` layout)").clicked() {
                self.start(&ctx, Action::CleanupPreview, Answers::default());
            }
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;
        egui::CollapsingHeader::new("Session").default_open(true).show(ui, |ui| {
            egui::Grid::new("session").num_columns(2).show(ui, |ui| {
                ui.label("Steam");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.steam, Steam::Gbe, "gbe emulator (offline, no account)");
                    ui.radio_value(&mut s.steam, Steam::Client, "my Steam client");
                });
                ui.end_row();
                ui.label("Workshop Tools");
                ui.add_enabled(cfg!(windows), egui::Checkbox::new(&mut s.tools, "-tools (Hammer, ModelDoc)")).on_disabled_hover_text("Windows only");
                ui.end_row();
                ui.label("Map");
                ui.add(egui::TextEdit::singleline(&mut s.map).hint_text("de_dust2 (client sessions start the bridge once a map runs)"));
                ui.end_row();
                ui.label("Addon");
                ui.add(egui::TextEdit::singleline(&mut s.addon).hint_text("-addon (tools)"));
                ui.end_row();
                ui.label("Extra game args");
                ui.add(egui::TextEdit::singleline(&mut s.extra).hint_text("-insecure is always added; -secure is refused"));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut s.capture, "capture layer (screenshot/observe)");
                ui.end_row();
            });
            if s.steam == Steam::Client {
                ui.colored_label(
                    severity_color(Severity::Warn),
                    "With your Steam client the game always runs with -insecure: VAC off, no official or secure servers (it carries injected code).",
                );
            }
        });
        egui::CollapsingHeader::new("Install and state").default_open(false).show(ui, |ui| {
            egui::Grid::new("paths").num_columns(2).show(ui, |ui| {
                ui.label("Game root");
                ui.add(egui::TextEdit::singleline(&mut s.game_root).hint_text("found in Steam's libraries").desired_width(520.0));
                ui.end_row();
                ui.label("Extra Steam library");
                ui.add(egui::TextEdit::singleline(&mut s.library).hint_text("e.g. D:\\SteamLibrary").desired_width(520.0));
                ui.end_row();
                ui.label("State dir");
                ui.add(egui::TextEdit::singleline(&mut s.state_dir).hint_text("default: %LOCALAPPDATA% / $XDG_STATE_HOME").desired_width(520.0));
                ui.end_row();
                ui.label("Strategy");
                egui::ComboBox::from_id_salt("strategy").selected_text(s.strategy.cli()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.strategy, Strategy::Auto, "auto");
                    if cfg!(target_os = "linux") {
                        ui.selectable_value(&mut s.strategy, Strategy::Overlay, "overlay (the install is never written)");
                    }
                    ui.selectable_value(&mut s.strategy, Strategy::InPlace, "in-place (manifest + backups, restored on Stop)");
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
        let running = self.job.as_ref().is_some_and(|j| matches!(j.action, Action::Up | Action::Wait));
        if self.stages.is_empty() && !running {
            return;
        }
        ui.separator();
        ui.heading("Readiness");
        // a read-only checkbox per stage: drawn, not a glyph (the bundled fonts have no ✔, it
        // rendered as a box on Windows), and screen readers get its state
        for (key, label) in STAGES {
            let reached = self.stages.iter().find(|(s, _)| s == key).map(|(_, secs)| *secs);
            let mut done = reached.is_some();
            let text = match reached {
                Some(secs) => RichText::new(format!("{label}  ({secs:.1}s)")),
                None => RichText::new(label.to_string()).weak(),
            };
            ui.add_enabled(false, egui::Checkbox::new(&mut done, text));
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
        let mut next: Option<(Action, Answers)> = None;
        let mut then: Option<Dialog> = None;
        let mut resume: Option<Option<(Action, Answers)>> = None;
        let mut restore_and_close = false;
        egui::Modal::new(egui::Id::unique("cs2mcp-dialog")).show(ctx, |ui| {
            ui.set_max_width(760.0);
            match d {
                Dialog::Findings { action, answers, title, findings, stopped } => {
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
                            next = Some((*action, Answers { accept_conflicts: true, ..*answers }));
                            close = true;
                        }
                        if (has(Code::Orphans) || has(Code::Legacy) || has(Code::ReplacedGameFile))
                            && ui.button("Clean up…").on_hover_text("cs2mcp-native cleanup (a preview), then cleanup --yes, then this action again").clicked()
                        {
                            next = Some((Action::CleanupPreview, Answers::default()));
                            resume = Some(stopped.then_some((*action, *answers)));
                            close = true;
                        }
                        if has(Code::MissingBackups) && ui.button("Mark as lost…").clicked() {
                            let paths = findings.iter().filter(|f| f.code == Code::MissingBackups).flat_map(|f| f.paths.clone()).collect();
                            then = Some(Dialog::ConfirmLost { action: *action, answers: *answers, paths });
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
                        if ui.button("Move them out").on_hover_text("cs2mcp-native cleanup --yes").clicked() {
                            next = Some((Action::Cleanup, Answers::default()));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            resume = Some(None);
                            close = true;
                        }
                    });
                }
                Dialog::ConfirmLost { action, answers, paths } => {
                    ui.heading("Mark the missing backups as lost?");
                    ui.label("The launcher's copies of these game files are gone. Everything else is restored; our bytes are removed from these, and Valve's own come back only through Steam:");
                    for p in paths {
                        ui.monospace(p);
                    }
                    ui.label(RichText::new(cs2_native::findings::VERIFY_ADVICE).italics());
                    ui.horizontal(|ui| {
                        if ui.button("Mark as lost and restore the rest").on_hover_text("--forget-missing-backups").clicked() {
                            next = Some((*action, Answers { forget_missing_backups: true, ..*answers }));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::ConfirmClose => {
                    ui.heading("Restore the install before closing?");
                    ui.label("The install is prepared or the game is running. Closing runs Stop first (cs2mcp-native down: stop the game, restore every file, verify), then the window closes. The install is never left prepared because the window closed.");
                    ui.horizontal(|ui| {
                        if ui.button("Stop, restore and close").clicked() {
                            restore_and_close = true;
                            close = true;
                        }
                        if ui.button("Keep the window open").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Error { title, text } => {
                    ui.heading(title);
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| ui.monospace(text));
                    ui.horizontal(|ui| {
                        if ui.button("Copy details").clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                        if ui.button("Close").clicked() {
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
        if let Some(r) = resume {
            self.resume = r;
        }
        if let Some((a, p)) = next {
            self.start(ctx, a, p);
        }
        if restore_and_close {
            self.closing = true;
            self.stop(ctx);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if ctx.input(|i| i.viewport().close_requested()) && on_close(self.close_ok, self.busy() || self.closing, self.status.as_ref()) == OnClose::Ask {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if !self.closing {
                self.dialog = Some(Dialog::ConfirmClose);
            }
        }
        if self.close_ok {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::Panel::top("top").show(ui, |ui| self.top(ui));
        egui::Panel::bottom("log").resizable(true).default_size(220.0).show(ui, |ui| self.log_ui(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
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
        if self.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn closing_asks_while_anything_is_applied() {
        let clean = json!({"manifest": null, "session": null});
        let prepared = json!({"manifest": {"state": "applied"}, "session": null});
        let running = json!({"manifest": null, "session": {"alive": true}});
        let exited = json!({"manifest": null, "session": {"alive": false}});
        assert_eq!(on_close(false, false, Some(&clean)), OnClose::Close);
        assert_eq!(on_close(false, false, Some(&exited)), OnClose::Close);
        assert_eq!(on_close(false, false, Some(&prepared)), OnClose::Ask);
        assert_eq!(on_close(false, false, Some(&running)), OnClose::Ask);
        // a command running, or no status read yet: ask
        assert_eq!(on_close(false, true, Some(&clean)), OnClose::Ask);
        assert_eq!(on_close(false, false, None), OnClose::Ask);
        // after the restore verified
        assert_eq!(on_close(true, false, Some(&prepared)), OnClose::Close);
    }
}
