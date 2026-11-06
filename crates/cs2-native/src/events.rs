//! Progress reporting: every human-readable line the library produces goes through an [`Events`]
//! sink. The CLI's sink ([`Events::stderr`]) prints the lines exactly as `cs2mcp-native` always
//! has (`[cs2mcp-native] ...` on stderr); the GUI takes them from a channel
//! ([`Events::channel`]) and never blocks on them. Machine-readable results stay the commands'
//! JSON return values.

use serde::Serialize;
use std::sync::mpsc;
use std::sync::Arc;

/// One progress line.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Event {
    /// what the launcher is doing
    Info { text: String },
    /// something the user should look at (drift, a conflict, a leftover)
    Warn { text: String },
    /// a readiness stage was reached (`ProcessUp`, ..., `BridgeHello`)
    Stage { stage: String, seconds: f64 },
}

impl Event {
    /// The line the CLI prints (without the `[cs2mcp-native] ` prefix).
    pub fn line(&self) -> String {
        match self {
            Event::Info { text } | Event::Warn { text } => text.clone(),
            Event::Stage { stage, seconds } => format!("ready: {stage} after {seconds:.1}s"),
        }
    }
}

/// Where [`Event`]s go. Cheap to clone; usable from any thread.
#[derive(Clone)]
pub struct Events(Arc<dyn Fn(Event) + Send + Sync>);

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Events")
    }
}

impl Events {
    pub fn new(f: impl Fn(Event) + Send + Sync + 'static) -> Events {
        Events(Arc::new(f))
    }

    /// The CLI sink: `[cs2mcp-native] <line>` on stderr.
    pub fn stderr() -> Events {
        Events::new(|e| eprintln!("[cs2mcp-native] {}", e.line()))
    }

    /// Drop everything (tests).
    pub fn null() -> Events {
        Events::new(|_| {})
    }

    /// A sink feeding a channel; the receiver side polls with `try_recv`.
    pub fn channel() -> (Events, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        (
            Events::new(move |e| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(e);
                }
            }),
            rx,
        )
    }

    /// Both sinks (the GUI also keeps a log file, for instance).
    pub fn tee(&self, other: Events) -> Events {
        let a = self.clone();
        Events::new(move |e| {
            a.emit(e.clone());
            other.emit(e);
        })
    }

    pub fn emit(&self, e: Event) {
        (self.0)(e)
    }

    pub fn info(&self, text: impl Into<String>) {
        self.emit(Event::Info { text: text.into() })
    }

    pub fn warn(&self, text: impl Into<String>) {
        self.emit(Event::Warn { text: text.into() })
    }

    pub fn stage(&self, stage: &str, seconds: f64) {
        self.emit(Event::Stage { stage: stage.to_string(), seconds })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_match_the_cli_and_the_channel_delivers_in_order() {
        assert_eq!(Event::Stage { stage: "BridgeHello".into(), seconds: 49.14 }.line(), "ready: BridgeHello after 49.1s");
        let (ev, rx) = Events::channel();
        ev.info("a");
        ev.warn("b");
        ev.stage("ProcessUp", 0.5);
        let got: Vec<Event> = rx.try_iter().collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1], Event::Warn { text: "b".into() });
    }
}
