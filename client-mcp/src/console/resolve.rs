//! Which console.log the console tools read.
//!
//! Readers (`console_tail`, `console_wait`). Precedence, first match wins:
//! 1. `--console-log` (the flag, `ClientState::console_log`): source `flag`; a `session` argument is
//!    then ignored and the result says so;
//! 2. the named session (`session` argument, any `cs2mcp-*` container, running or not): source `session`;
//! 3. exactly one running session with a known console.log: source `single-session`;
//! 4. several running sessions and no `session` argument: `InvalidArguments` listing them;
//! 5. otherwise the default path `<project>/workshop/run/app/game/csgo/console.log`: source `default`.
//!
//! The bridge's log (`console_exec`'s cursor, `Purpose::Bridge`): the flag, the named session, then
//! the running wine-tools session (source `wine-session`), else the default. It never takes the
//! generic single-session pick and never fails on an ambiguous set of sessions.
//!
//! A Linux session's log is `<artifacts>/console.log` (mirrored by the launch script); a Wine
//! session's is `<app>/game/csgo/console.log`. Both come from `SessionEntry::console_log`.
use crate::state::ClientState;
use crate::tools::runtime_tools::runtime_error;
use cs2_mcp_common::config::RuntimeKind;
use cs2_mcp_common::runtime::docker::{valid_name, SessionEntry};
use cs2_mcp_common::tools::{CallCtx, ToolError};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Kept back from the call budget so the reply still goes out in time.
const BUDGET_MARGIN: Duration = Duration::from_millis(500);

/// Whose console.log is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// `console_tail` / `console_wait`: an ambiguous set is an error (the caller must pick)
    Reader,
    /// `console_exec`'s cursor: the bridge's session, never the generic pick
    Bridge,
}

/// The chosen log and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLog {
    pub path: PathBuf,
    /// `flag` | `session` | `single-session` | `wine-session` | `default` | `cursor`
    pub source: &'static str,
    /// the session container the path belongs to (when one does)
    pub session: Option<String>,
    pub note: Option<String>,
}

fn resolved(path: PathBuf, source: &'static str, session: Option<String>) -> ResolvedLog {
    ResolvedLog {
        path,
        source,
        session,
        note: None,
    }
}

fn log_of(entry: &SessionEntry) -> Result<PathBuf, ToolError> {
    entry.console_log.clone().ok_or_else(|| {
        ToolError::invalid_arguments(format!(
            "session {} has no console.log path this server knows how to read",
            entry.name
        ))
        .with_data(json!({"session": entry.name, "note": entry.note}))
    })
}

/// The reader precedence over an already-listed set of sessions (`sessions`: every candidate
/// container, any state; only the running ones are candidates for the automatic pick).
pub fn choose(
    flag: Option<&Path>,
    default: &Path,
    session: Option<&str>,
    sessions: &[SessionEntry],
) -> Result<ResolvedLog, ToolError> {
    if let Some(path) = flag {
        let note =
            session.map(|_| "session argument ignored: --console-log is set and wins".to_string());
        return Ok(ResolvedLog {
            note,
            ..resolved(path.to_path_buf(), "flag", None)
        });
    }
    if let Some(name) = session {
        let entry = sessions.iter().find(|e| e.name == name).ok_or_else(|| {
            ToolError::invalid_arguments(format!(
                "no cs2mcp session named {name:?} (session_status lists them)"
            ))
            .with_data(json!({"session": name}))
        })?;
        return Ok(resolved(log_of(entry)?, "session", Some(name.to_string())));
    }
    let running: Vec<&SessionEntry> = sessions
        .iter()
        .filter(|e| e.running() && e.console_log.is_some())
        .collect();
    match running.as_slice() {
        [] => Ok(resolved(default.to_path_buf(), "default", None)),
        [one] => Ok(resolved(
            log_of(one)?,
            "single-session",
            Some(one.name.clone()),
        )),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name.as_str()).collect();
            Err(ToolError::invalid_arguments(format!(
                "{} sessions are running ({}): pass session: <container> to pick the console.log",
                names.len(),
                names.join(", ")
            ))
            .with_data(json!({"sessions": names})))
        }
    }
}

/// The bridge's precedence (`console_exec`): flag, named session, the one running Wine session
/// (wine-tools or wine-client; both share the app tree's console.log), else the default. Never
/// ambiguous: a second Wine session (not expected, the runtime refuses it) falls back to the
/// default with a note.
pub fn choose_bridge(
    flag: Option<&Path>,
    default: &Path,
    session: Option<&str>,
    sessions: &[SessionEntry],
) -> Result<ResolvedLog, ToolError> {
    if flag.is_some() || session.is_some() {
        return choose(flag, default, session, sessions);
    }
    let wine: Vec<&SessionEntry> = sessions
        .iter()
        .filter(|e| {
            e.running()
                && (e.kind == "wine-tools" || e.kind == "wine-client")
                && e.console_log.is_some()
        })
        .collect();
    match wine.as_slice() {
        [] => Ok(resolved(default.to_path_buf(), "default", None)),
        [one] => Ok(resolved(
            log_of(one)?,
            "wine-session",
            Some(one.name.clone()),
        )),
        many => Ok(ResolvedLog {
            note: Some(format!(
                "{} Wine sessions are running; the default console.log is used",
                many.len()
            )),
            ..resolved(default.to_path_buf(), "default", None)
        }),
    }
}

/// The optional `session` argument of the console tools (`None` when absent or null).
pub fn session_arg(args: &serde_json::Value) -> Result<Option<String>, ToolError> {
    match args.get("session") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(ToolError::invalid_arguments(
            "session must be a non-empty cs2mcp-* container name",
        )),
    }
}

/// Add the resolution fields to a tool result: `source`, and `session` / `sourceNote` when set.
pub fn describe(mut result: serde_json::Value, resolved: &ResolvedLog) -> serde_json::Value {
    result["source"] = json!(resolved.source);
    if let Some(s) = &resolved.session {
        result["session"] = json!(s);
    }
    if let Some(n) = &resolved.note {
        result["sourceNote"] = json!(n);
    }
    result
}

/// The live resolution. Only what the answer needs is inspected: a named session is looked up by
/// name, and without a name the (cached, running-only) session list is used.
async fn resolve_inner(
    state: &ClientState,
    session: Option<&str>,
    purpose: Purpose,
) -> Result<ResolvedLog, ToolError> {
    if let Some(name) = session {
        if !valid_name(name) {
            return Err(ToolError::invalid_arguments(format!(
                "session must be a cs2mcp-* container name, got {name:?}"
            )));
        }
    }
    let docker = state.runtime_kind == RuntimeKind::Docker;
    if state.console_log.is_some() || (session.is_none() && !docker) {
        return match purpose {
            Purpose::Reader => choose(
                state.console_log.as_deref(),
                &state.default_console_log,
                session,
                &[],
            ),
            Purpose::Bridge => choose_bridge(
                state.console_log.as_deref(),
                &state.default_console_log,
                session,
                &[],
            ),
        };
    }
    if let Some(name) = session {
        let entry = if docker {
            state
                .runtime
                .session_entry(name)
                .await
                .map_err(runtime_error)?
        } else {
            None
        };
        return choose(
            None,
            &state.default_console_log,
            Some(name),
            &entry.into_iter().collect::<Vec<_>>(),
        );
    }
    let (sessions, note) = match state.runtime.list_sessions().await {
        Ok(list) => (list, None),
        Err(e) => (Vec::new(), Some(format!("session discovery failed: {e:#}"))),
    };
    let mut resolved = match purpose {
        Purpose::Reader => choose(None, &state.default_console_log, None, &sessions)?,
        Purpose::Bridge => choose_bridge(None, &state.default_console_log, None, &sessions)?,
    };
    if resolved.note.is_none() {
        resolved.note = note;
    }
    Ok(resolved)
}

/// [`resolve_inner`] bounded by the call budget (minus [`BUDGET_MARGIN`]): podman is never allowed
/// to use up the time the reply needs. Out of time is `ToolBudgetExceeded`.
pub async fn resolve_within(
    state: &ClientState,
    ctx: &CallCtx,
    session: Option<&str>,
    purpose: Purpose,
) -> Result<ResolvedLog, ToolError> {
    let room = ctx
        .budget
        .remaining()
        .checked_sub(BUDGET_MARGIN)
        .filter(|d| !d.is_zero())
        .ok_or_else(|| ToolError::budget_exceeded(&ctx.budget))?;
    match tokio::time::timeout(room, resolve_inner(state, session, purpose)).await {
        Ok(result) => result,
        Err(_) => Err(ToolError::budget_exceeded(&ctx.budget)),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    fn entry(name: &str, state: &str, console: Option<&str>) -> SessionEntry {
        let kind = if name.contains("wine-tools") {
            "wine-tools"
        } else {
            "linux-client"
        };
        SessionEntry {
            name: name.into(),
            kind: kind.into(),
            state: state.into(),
            owned: false,
            artifacts_dir: None,
            console_log: console.map(PathBuf::from),
            app_dir: None,
            note: None,
            map: None,
            addon: None,
        }
    }

    const DEFAULT: &str = "/project/workshop/run/app/game/csgo/console.log";

    #[test]
    fn flag_beats_session_argument_and_running_sessions() {
        let sessions = [entry(
            "cs2mcp-linux-client-a",
            "running",
            Some("/art/a/console.log"),
        )];
        let r = choose(
            Some(Path::new("/flag.log")),
            Path::new(DEFAULT),
            Some("cs2mcp-linux-client-a"),
            &sessions,
        )
        .unwrap();
        assert_eq!(r.source, "flag");
        assert_eq!(r.path, PathBuf::from("/flag.log"));
        assert!(r.note.unwrap().contains("ignored"));
    }

    #[test]
    fn named_session_beats_single_running_and_may_be_stopped() {
        let sessions = [
            entry(
                "cs2mcp-linux-client-a",
                "running",
                Some("/art/a/console.log"),
            ),
            entry(
                "cs2mcp-wine-tools-b",
                "exited",
                Some("/app/game/csgo/console.log"),
            ),
        ];
        let r = choose(
            None,
            Path::new(DEFAULT),
            Some("cs2mcp-wine-tools-b"),
            &sessions,
        )
        .unwrap();
        assert_eq!(r.source, "session");
        assert_eq!(r.session.as_deref(), Some("cs2mcp-wine-tools-b"));
        assert_eq!(r.path, PathBuf::from("/app/game/csgo/console.log"));
    }

    #[test]
    fn unknown_session_name_is_invalid_arguments() {
        let err = choose(None, Path::new(DEFAULT), Some("cs2mcp-nope-1"), &[]).unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
    }

    #[test]
    fn single_running_session_beats_default() {
        let sessions = [
            entry(
                "cs2mcp-linux-client-a",
                "exited",
                Some("/art/old/console.log"),
            ),
            entry(
                "cs2mcp-linux-client-b",
                "running",
                Some("/art/b/console.log"),
            ),
        ];
        let r = choose(None, Path::new(DEFAULT), None, &sessions).unwrap();
        assert_eq!(r.source, "single-session");
        assert_eq!(r.path, PathBuf::from("/art/b/console.log"));
    }

    #[test]
    fn several_running_sessions_need_a_name() {
        let sessions = [
            entry(
                "cs2mcp-linux-client-a",
                "running",
                Some("/art/a/console.log"),
            ),
            entry(
                "cs2mcp-wine-tools-b",
                "running",
                Some("/app/game/csgo/console.log"),
            ),
        ];
        let err = choose(None, Path::new(DEFAULT), None, &sessions).unwrap_err();
        assert_eq!(err.type_, "InvalidArguments");
        assert!(err.message.contains("cs2mcp-linux-client-a"));
        assert!(err.message.contains("cs2mcp-wine-tools-b"));
        assert_eq!(err.data["sessions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn no_running_session_falls_back_to_default() {
        let sessions = [entry(
            "cs2mcp-linux-client-a",
            "exited",
            Some("/art/a/console.log"),
        )];
        let r = choose(None, Path::new(DEFAULT), None, &sessions).unwrap();
        assert_eq!(r.source, "default");
        assert_eq!(r.path, PathBuf::from(DEFAULT));
        assert_eq!(r.session, None);
    }

    #[test]
    fn bridge_takes_the_running_wine_session_even_with_other_sessions_running() {
        let sessions = [
            entry(
                "cs2mcp-linux-client-a",
                "running",
                Some("/art/a/console.log"),
            ),
            entry(
                "cs2mcp-wine-tools-b",
                "running",
                Some("/app/game/csgo/console.log"),
            ),
        ];
        let r = choose_bridge(None, Path::new(DEFAULT), None, &sessions).unwrap();
        assert_eq!(r.source, "wine-session");
        assert_eq!(r.path, PathBuf::from("/app/game/csgo/console.log"));
    }

    #[test]
    fn bridge_never_takes_the_single_linux_session_and_never_errors_on_several() {
        let one = [entry(
            "cs2mcp-linux-client-a",
            "running",
            Some("/art/a/console.log"),
        )];
        let r = choose_bridge(None, Path::new(DEFAULT), None, &one).unwrap();
        assert_eq!(r.source, "default");
        let two = [
            entry(
                "cs2mcp-linux-client-a",
                "running",
                Some("/art/a/console.log"),
            ),
            entry(
                "cs2mcp-linux-client-b",
                "running",
                Some("/art/b/console.log"),
            ),
        ];
        let r = choose_bridge(None, Path::new(DEFAULT), None, &two).unwrap();
        assert_eq!(r.source, "default");
        assert_eq!(r.path, PathBuf::from(DEFAULT));
    }
}
