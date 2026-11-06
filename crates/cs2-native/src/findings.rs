//! What the launcher found that differs from what it expected: a crash it recovered from, files
//! Steam changed under a session, backups that are gone, leftovers of an older install, a payload
//! that does not match its `SHA256SUMS`. Every case is reported as a [`Finding`] (in the JSON
//! result, as a dialog in the GUI); none is resolved by guessing.
//!
//! [`Severity`] decides what happens next:
//! * `info` / `warn`: reported, the command goes on;
//! * `decision`: the user has to choose. Before anything changes (`prepare`, `up`, `run`) the
//!   command stops with exit code 2 unless `--accept-conflicts` (GUI: "Continue anyway"); after a
//!   restore (`down`, `uninstall`, `run`) the work is done and the exit code is 2 unless
//!   `--accept-conflicts`;
//! * `blocker`: refused. Only the flag the finding names resolves it (`--forget-missing-backups`),
//!   or nothing does (a tampered payload).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Decision,
    Blocker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Code {
    /// an unfinished or stale manifest was restored before the new prepare
    Recovered,
    /// files changed by someone else since we wrote them (a Steam update or "Verify integrity")
    Conflicts,
    /// the manifest's backups of the user's files are missing or damaged
    MissingBackups,
    /// backups declared lost by the user (`--forget-missing-backups`)
    LostBackups,
    /// restore did not verify
    VerifyFailed,
    /// the game build is not the one the shipped signatures/pins target
    BuildMismatch,
    /// files of an older install layout (`cargo bridge install` / install-rs.sh, the image's setup_gbe.sh)
    Legacy,
    /// our files in a tree that has no manifest (the state dir is gone, or a `cargo bridge install` one)
    Orphans,
    /// a Valve file replaced or edited outside a manifest
    ReplacedGameFile,
    /// the payload does not match its SHA256SUMS
    PayloadTampered,
    /// the payload has no SHA256SUMS (a development tree)
    PayloadUnverified,
    /// more than one CS2 install was found
    MultipleInstalls,
}

/// The advice that goes with every "Steam changed it" case.
pub const VERIFY_ADVICE: &str =
    "In Steam: Library > Counter-Strike 2 > Properties > Installed Files > Verify integrity of game files";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub code: Code,
    pub severity: Severity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

impl Finding {
    pub fn new(code: Code, severity: Severity, title: impl Into<String>) -> Finding {
        Finding { code, severity, title: title.into(), paths: Vec::new(), advice: Vec::new() }
    }
    pub fn paths(mut self, p: impl IntoIterator<Item = impl Into<String>>) -> Finding {
        self.paths.extend(p.into_iter().map(Into::into));
        self
    }
    pub fn advice(mut self, a: impl Into<String>) -> Finding {
        self.advice.push(a.into());
        self
    }
    /// The finding as progress lines (CLI stderr / GUI log).
    pub fn report(&self, ev: &crate::events::Events) {
        let line = format!("{}: {}", self.severity_word(), self.title);
        if self.severity >= Severity::Warn {
            ev.warn(line);
        } else {
            ev.info(line);
        }
        for p in &self.paths {
            ev.info(format!("  {p}"));
        }
        for a in &self.advice {
            ev.info(format!("  -> {a}"));
        }
    }
    fn severity_word(&self) -> &'static str {
        match self.severity {
            Severity::Info => "note",
            Severity::Warn => "warning",
            Severity::Decision => "needs your decision",
            Severity::Blocker => "refused",
        }
    }
}

/// How the user answered the decisions in advance (CLI flags; GUI dialog buttons).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `--accept-conflicts`: go on past `decision` findings (keep leftovers as the tree's own,
    /// accept a restore that left Steam's newer files in place)
    pub accept_conflicts: bool,
    /// `--forget-missing-backups`: restore what can be restored and record the rest as lost
    pub forget_missing_backups: bool,
}

/// The error a command returns when it stopped on findings: the CLI prints them as JSON and
/// exits 2, the GUI shows the dialog.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub message: String,
    pub findings: Vec<Finding>,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Stopped {}

/// Stop with `message` when `findings` hold a blocker, or a decision the policy did not accept.
pub fn gate(findings: &[Finding], policy: Policy, message: &str) -> Result<(), Stopped> {
    let stop = findings.iter().any(|f| match f.severity {
        Severity::Blocker => true,
        Severity::Decision => !policy.accept_conflicts,
        _ => false,
    });
    if stop {
        return Err(Stopped { message: message.to_string(), findings: findings.to_vec() });
    }
    Ok(())
}

/// The exit code for a finished command: 2 when a decision is left open (not accepted) or a
/// blocker was reported, else 0.
pub fn exit_code(findings: &[Finding], policy: Policy) -> i32 {
    let open = findings.iter().any(|f| f.severity == Severity::Blocker || (f.severity == Severity::Decision && !policy.accept_conflicts));
    if open {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_stop_unless_accepted_and_blockers_always_stop() {
        let warn = Finding::new(Code::BuildMismatch, Severity::Warn, "w");
        let dec = Finding::new(Code::Legacy, Severity::Decision, "d");
        let blk = Finding::new(Code::PayloadTampered, Severity::Blocker, "b");
        let accept = Policy { accept_conflicts: true, ..Default::default() };
        assert!(gate(std::slice::from_ref(&warn), Policy::default(), "x").is_ok());
        assert!(gate(&[warn.clone(), dec.clone()], Policy::default(), "x").is_err());
        assert!(gate(&[warn.clone(), dec.clone()], accept, "x").is_ok());
        assert!(gate(std::slice::from_ref(&blk), accept, "x").is_err());
        assert_eq!(exit_code(std::slice::from_ref(&dec), Policy::default()), 2);
        assert_eq!(exit_code(&[dec], accept), 0);
        assert_eq!(exit_code(&[warn], Policy::default()), 0);
        let v = serde_json::to_value(&blk).unwrap();
        assert_eq!(v["code"], "payload-tampered");
        assert_eq!(v["severity"], "blocker");
    }
}
