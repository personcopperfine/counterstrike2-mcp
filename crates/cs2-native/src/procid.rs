//! Which process a recorded pid was. A pid alone is not enough: after a reboot, or in a new PID
//! namespace (a container started again on the same volume, where the state dir lives), the pid
//! in an old `session.json` can belong to an unrelated live process, and `down` would kill it or
//! refuse to restore. So every pid is recorded with its identity: the process start time, the
//! boot id and the PID namespace (Linux), the creation time (Windows). A recorded pid counts as
//! alive only while all of them still match.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcIdentity {
    /// Linux: `/proc/<pid>/stat` starttime (clock ticks since boot); Windows: the creation time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    /// Linux: `/proc/sys/kernel/random/boot_id` when it was recorded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Linux: the recorder's PID namespace (`/proc/self/ns/pid`, e.g. `pid:[4026531836]`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_ns: Option<String>,
}

/// The identity of `pid` now, from this process's point of view.
pub fn of(pid: u32) -> ProcIdentity {
    #[cfg(target_os = "linux")]
    {
        ProcIdentity { start: linux_start(pid), boot_id: boot_id(), pid_ns: pid_ns() }
    }
    #[cfg(windows)]
    {
        ProcIdentity { start: crate::win::process_start(pid), boot_id: None, pid_ns: None }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        ProcIdentity::default()
    }
}

/// Is `recorded` (taken when the pid was saved) the same process as `now` (taken for the same pid
/// today)? A field recorded but different, or recorded but unreadable now, means another process
/// (or another namespace, where the number means nothing): not the same. A field never recorded
/// (an older session file) is not held against it.
pub fn same(recorded: &ProcIdentity, now: &ProcIdentity) -> bool {
    fn ok<T: PartialEq>(r: &Option<T>, n: &Option<T>) -> bool {
        match (r, n) {
            (None, _) => true,
            (Some(r), Some(n)) => r == n,
            (Some(_), None) => false,
        }
    }
    ok(&recorded.boot_id, &now.boot_id) && ok(&recorded.pid_ns, &now.pid_ns) && ok(&recorded.start, &now.start)
}

#[cfg(target_os = "linux")]
fn linux_start(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_start(&s)
}

/// Field 22 (`starttime`) of a `/proc/<pid>/stat` line. The command name (field 2) may hold
/// spaces and parentheses, so the fields are counted after its last `)`.
pub fn parse_stat_start(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    // rest starts at field 3 (state); starttime is field 22
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(target_os = "linux")]
fn pid_ns() -> Option<String> {
    std::fs::read_link("/proc/self/ns/pid").ok().map(|p| p.to_string_lossy().into_owned())
}

/// Is `pid` alive and still the process `recorded` names?
pub fn alive_as(pid: u32, recorded: Option<&ProcIdentity>) -> bool {
    if !crate::session::pid_alive(pid) {
        return false;
    }
    match recorded {
        Some(r) => same(r, &of(pid)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_start_survives_odd_command_names() {
        let line = "1234 (cs2 (x) y) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 30 0 987654 1000 200 18446744073709551615";
        assert_eq!(parse_stat_start(line), Some(987654));
        assert_eq!(parse_stat_start("garbage"), None);
    }

    #[test]
    fn a_changed_boot_namespace_or_start_time_is_another_process() {
        let r = ProcIdentity { start: Some(10), boot_id: Some("b1".into()), pid_ns: Some("pid:[1]".into()) };
        assert!(same(&r, &r.clone()));
        assert!(!same(&r, &ProcIdentity { boot_id: Some("b2".into()), ..r.clone() }), "rebooted");
        assert!(!same(&r, &ProcIdentity { pid_ns: Some("pid:[2]".into()), ..r.clone() }), "another container");
        assert!(!same(&r, &ProcIdentity { start: Some(11), ..r.clone() }), "pid reused");
        assert!(!same(&r, &ProcIdentity { start: None, ..r.clone() }), "unreadable now");
        assert!(same(&ProcIdentity::default(), &r), "an old session file without identity");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_itself_and_a_dead_child_is_not_alive() {
        let me = std::process::id();
        let id = of(me);
        assert!(id.start.is_some() && id.boot_id.is_some() && id.pid_ns.is_some(), "{id:?}");
        assert!(alive_as(me, Some(&id)));
        assert!(!alive_as(me, Some(&ProcIdentity { start: id.start.map(|s| s + 1), ..id.clone() })));
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        assert!(!alive_as(pid, None));
    }
}
