//! Fake `podman` for tests: a shell script on a temp dir that records every invocation and keeps
//! container state as files. `run --name X` creates `state/X` (content `running`), `rm X` removes
//! it (refused while it is running or stopping, like podman without `-f`), `ps` lists the running
//! ones as JSON (`ps -a`: all of them, with their state word), `inspect` prints the state word,
//! `logs` prints `state/X.log`. `stop` sets `stopping` and reports `exited 137` after
//! `slow-stop-X` seconds (default 0) in the background; `wait --condition=exited X` polls until
//! that. `inspect` of a container with a `slow-inspect-X` marker stalls once. `exec` prints
//! `exec-out-X` (else `exec-ok`) and exits with `exec-rc-X` (else 0).
//! Never touches a real container runtime.
//!
//! Call-log encoding: every argument is written hex-encoded (`x<hex>`), so no separator can appear
//! inside an argument. Records end with a newline; fields are space-separated.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct FakePodman {
    dir: PathBuf,
    pub bin: PathBuf,
    state: PathBuf,
    log: PathBuf,
}

/// One recorded invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// the `XDG_RUNTIME_DIR` the call ran with
    pub xdg_runtime_dir: String,
    /// podman arguments (no binary), decoded
    pub argv: Vec<String>,
}

impl FakePodman {
    pub fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "fakepodman-{tag}-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let state = dir.join("state");
        let log = dir.join("calls.log");
        std::fs::create_dir_all(&state).expect("fake podman state dir");
        let bin = dir.join("podman");
        let script = format!(
            r#"#!/bin/sh
STATE='{state}'
LOG='{log}'
DIR='{dir}'
enc() {{ printf 'x'; printf '%s' "$1" | od -An -v -tx1 | tr -d ' \n'; }}
# one record, one write: a detached `stop` and a `wait` may log at the same time
rec="$(enc "XDG=$XDG_RUNTIME_DIR")"
for a in "$@"; do rec="$rec $(enc "$a")"; done
printf '%s\n' "$rec" >> "$LOG"
if [ -f "$DIR/fail-$1" ]; then echo "fake podman: $1 is failing (fail-$1)" >&2; exit 125; fi
last=''
for a in "$@"; do last="$a"; done
case "$1" in
  ps)
    all=0
    for a in "$@"; do [ "$a" = "-a" ] && all=1; done
    printf '['
    sep=''
    for f in "$STATE"/*; do
      [ -f "$f" ] || continue
      n=$(basename "$f")
      case "$n" in *.*) continue ;; esac
      st=$(cat "$f")
      if [ "$all" = 0 ]; then
        case "$st" in running*) ;; *) continue ;; esac
      fi
      word=${{st%% *}}
      printf '%s{{"Id":"%s","Names":["%s"],"State":"%s"}}' "$sep" "$n" "$n" "$word"
      sep=','
    done
    printf ']\n'
    ;;
  run)
    name=''; prev=''; mounts='['; msep=''
    for a in "$@"; do
      if [ "$prev" = "--name" ]; then name="$a"; fi
      if [ "$prev" = "-v" ]; then
        src="${{a%%:*}}"; rest="${{a#*:}}"; dst="${{rest%%:*}}"
        case "$src" in
          /*) typ=bind; nm=''; srcp="$src" ;;
          *) typ=volume; nm="$src"; srcp="/var/lib/containers/storage/volumes/$src/_data" ;;
        esac
        mounts="$mounts$msep{{\"Type\":\"$typ\",\"Name\":\"$nm\",\"Source\":\"$srcp\",\"Destination\":\"$dst\"}}"
        msep=','
      fi
      prev="$a"
    done
    printf '%s]' "$mounts" > "$STATE/$name.mounts"
    printf 'running' > "$STATE/$name"
    echo "fakeid-$name"
    ;;
  stop)
    if [ ! -f "$STATE/$last" ]; then
      echo "Error: no container with name or ID \"$last\" found: no such container" >&2; exit 125
    fi
    printf 'stopping' > "$STATE/$last"
    delay=0
    if [ -f "$DIR/slow-stop-$last" ]; then delay=$(cat "$DIR/slow-stop-$last"); fi
    ( sleep "$delay"; printf 'exited 137' > "$STATE/$last" ) >/dev/null 2>&1 &
    echo "$last"
    ;;
  wait)
    while :; do
      if [ ! -f "$STATE/$last" ]; then
        echo "Error: no container with name or ID \"$last\" found: no such container" >&2; exit 125
      fi
      case "$(cat "$STATE/$last")" in exited*|stopped*) echo 137; exit 0 ;; esac
      sleep 0.05
    done
    ;;
  rm)
    shift
    rc=0
    for a in "$@"; do
      case "$a" in
        -*) ;;
        *)
          if [ -f "$STATE/$a" ]; then
            case "$(cat "$STATE/$a")" in
              running*|stopping*)
                echo "Error: cannot remove container $a: it is $(cat "$STATE/$a") (stop it first)" >&2
                rc=125
                continue ;;
            esac
            rm -f "$STATE/$a" "$STATE/$a.log" "$STATE/$a.mounts"
          else
            echo "Error: no container with name or ID \"$a\" found: no such container" >&2
            rc=1
          fi
          ;;
      esac
    done
    exit $rc
    ;;
  inspect)
    case "$*" in
      *Mounts*)
        # `inspect -f FORMAT name...`: one `name<TAB>mounts` line per container, like podman
        skip=0; rc=0
        for a in "$@"; do
          if [ "$skip" = 1 ]; then skip=0; continue; fi
          case "$a" in
            inspect) continue ;;
            -f) skip=1; continue ;;
          esac
          if [ -f "$DIR/inspect-fail-$a" ]; then
            echo "Error: inspect of $a failed (inspect-fail)" >&2
            rc=125
          elif [ -f "$STATE/$a" ]; then
            m='[]'
            if [ -f "$STATE/$a.mounts" ]; then m=$(cat "$STATE/$a.mounts"); fi
            printf '%s\t%s\n' "$a" "$m"
          else
            echo "Error: no container with name or ID \"$a\" found: no such container" >&2
            rc=125
          fi
        done
        exit $rc ;;
      *)
        # a stalled podman (its store's disk syncing): the next state inspect of this container
        # answers only after the delay; the marker is taken first, so only one inspect stalls
        if [ -f "$DIR/slow-inspect-$last" ]; then
          d=$(cat "$DIR/slow-inspect-$last"); rm -f "$DIR/slow-inspect-$last"; sleep "$d"
        fi
        # podman 5.x words a missing container this way for `inspect -f` (live, 2026-10-11)
        if [ -f "$STATE/$last" ]; then cat "$STATE/$last"; echo
        else echo "Error: no such object: \"$last\"" >&2; exit 125; fi ;;
    esac
    ;;
  exec)
    # `exec -i NAME argv...`: the helper output and exit status can be set per container
    date +%s%N >> "$DIR/exec-times"
    out="$DIR/exec-out-$3"; rcf="$DIR/exec-rc-$3"
    [ -f "$out" ] || out="$DIR/exec-out-default"
    [ -f "$rcf" ] || rcf="$DIR/exec-rc-default"
    if [ -f "$out" ]; then cat "$out"; else echo "exec-ok"; fi
    if [ -f "$rcf" ]; then exit "$(cat "$rcf")"; fi
    ;;
  logs)
    tailn=''; prev=''
    for a in "$@"; do
      if [ "$prev" = "--tail" ]; then tailn="$a"; fi
      prev="$a"
    done
    if [ -f "$STATE/$last.log" ]; then tail -n "${{tailn:-100000}}" "$STATE/$last.log"; else echo "fake log line"; fi ;;
  *) echo "fake podman: unsupported $1" >&2; exit 2 ;;
esac
"#,
            state = state.display(),
            log = log.display(),
            dir = dir.display(),
        );
        std::fs::write(&bin, script).expect("write fake podman");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake podman");
        }
        FakePodman {
            dir,
            bin,
            state,
            log,
        }
    }

    /// Pretend a container is running (e.g. a `cs2vtest` made by run_vtest.sh).
    pub fn add_running(&self, name: &str) {
        self.set_state(name, "running");
    }

    /// Set the state word `inspect` reports (`running`, or `exited <code>`) and make it exist.
    pub fn set_state(&self, name: &str, state: &str) {
        std::fs::write(self.state.join(name), state).expect("fake container");
    }

    /// Mounts `inspect -f {{json .Mounts}}` prints for a container (JSON array text).
    pub fn set_mounts(&self, name: &str, json: &str) {
        std::fs::write(self.state.join(format!("{name}.mounts")), json).expect("fake mounts");
    }

    /// Make `inspect` of this container fail (listed by `ps`, but its mounts cannot be read).
    pub fn fail_inspect(&self, name: &str) {
        std::fs::write(self.dir.join(format!("inspect-fail-{name}")), "").expect("inspect marker");
    }

    /// Make every later `verb` call fail (exit 125), e.g. `fail_verb("ps")`.
    pub fn fail_verb(&self, verb: &str) {
        std::fs::write(self.dir.join(format!("fail-{verb}")), "").expect("fail marker");
    }

    /// What `exec` prints for this container (vtmsend's output in tests), and its exit status.
    pub fn set_exec(&self, name: &str, stdout: &str, rc: i32) {
        std::fs::write(self.dir.join(format!("exec-out-{name}")), stdout).expect("exec output");
        std::fs::write(self.dir.join(format!("exec-rc-{name}")), rc.to_string()).expect("exec rc");
    }

    /// [`FakePodman::set_exec`] for every container that has none of its own (the generated names
    /// of launches made through a tool are not known to the test).
    pub fn set_exec_default(&self, stdout: &str, rc: i32) {
        std::fs::write(self.dir.join("exec-out-default"), stdout).expect("exec output");
        std::fs::write(self.dir.join("exec-rc-default"), rc.to_string()).expect("exec rc");
    }

    /// Seconds a `stop` of this container takes before it reports `exited` (default 0).
    pub fn set_slow_stop(&self, name: &str, secs: u32) {
        std::fs::write(self.dir.join(format!("slow-stop-{name}")), secs.to_string())
            .expect("slow stop");
    }

    /// The next state `inspect` of this container answers only after `secs` (once): a podman
    /// stalled on its storage, as seen live when the container store's disk syncs for tens of seconds.
    pub fn set_slow_inspect_once(&self, name: &str, secs: u32) {
        std::fs::write(
            self.dir.join(format!("slow-inspect-{name}")),
            secs.to_string(),
        )
        .expect("slow inspect");
    }

    /// Make a container disappear (as if it was removed outside this server).
    pub fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.state.join(name));
    }

    /// Text `logs` prints for a container.
    pub fn set_logs(&self, name: &str, text: &str) {
        std::fs::write(self.state.join(format!("{name}.log")), text).expect("fake logs");
    }

    /// True while the container exists and is in the `running` state.
    pub fn is_running(&self, name: &str) -> bool {
        std::fs::read_to_string(self.state.join(name))
            .map(|s| s.starts_with("running"))
            .unwrap_or(false)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.state.join(name).exists()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Wall-clock nanoseconds at which each `exec` ran, oldest first.
    pub fn exec_times(&self) -> Vec<u128> {
        std::fs::read_to_string(self.dir.join("exec-times"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// Every recorded invocation, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        let raw = std::fs::read_to_string(&self.log).unwrap_or_default();
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| {
                let mut fields = line.split(' ').filter(|f| !f.is_empty()).map(decode);
                let xdg = fields
                    .next()
                    .and_then(|x| x.strip_prefix("XDG=").map(String::from))
                    .unwrap_or_default();
                Call {
                    xdg_runtime_dir: xdg,
                    argv: fields.collect(),
                }
            })
            .collect()
    }

    /// Invocations whose first argument is `verb`.
    pub fn calls_to(&self, verb: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| c.argv.first().map(String::as_str) == Some(verb))
            .collect()
    }
}

fn decode(field: &str) -> String {
    let hex = field.strip_prefix('x').unwrap_or(field);
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(b'?'))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl Drop for FakePodman {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
