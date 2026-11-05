//! Host `console.log` access: a chunked backward tail, and a polling follower for `console_wait`
//! that survives truncation (`-conclearlog`: same inode, size shrinks, possibly regrows past the
//! old offset) and rotation (new inode on unix; the old file is drained before switching).
//!
//! Only complete `\n`-terminated lines are ever matched by the follower. The tail also returns a
//! final unterminated line (the engine's last partial write), because it is useful evidence.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Bytes read per backward step (grows when a single line is longer than this).
const CHUNK: u64 = 64 * 1024;
/// `console_tail` never reads more than this from the end (then `reachedStartOfFile` is false).
pub const TAIL_SCAN_CAP: u64 = 64 * 1024 * 1024;
/// Follower reads at most this much per step; the wait loop yields between steps.
const READ_CAP: u64 = 256 * 1024;
/// [`ConsoleLog::read_new_lines`] reads at most this much per call (the caller polls again).
pub const READ_NEW_CAP: u64 = 4 * 1024 * 1024;
/// A line longer than this is kept as its first bytes plus a `[truncated]` marker.
const PARTIAL_CAP: usize = 1024 * 1024;
/// Bytes kept before the read offset. A same-inode truncate-then-regrow changes them.
const SIG_LEN: usize = 64;
/// Follower poll interval while the file is idle.
const POLL: Duration = Duration::from_millis(50);
const TRUNCATED_MARK: &str = " [truncated]";

/// A position in `console.log`: the inode it refers to and a byte offset into it.
///
/// `path` names the file the position belongs to (set by `ConsoleLog::cursor` and `tail`). It is
/// optional on input so cursors from older callers still work: without it the cursor applies to
/// whichever file the tool resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    pub inode: u64,
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug)]
pub enum ConsoleError {
    Missing,
    Io(io::Error),
}

impl From<io::Error> for ConsoleError {
    fn from(e: io::Error) -> Self {
        ConsoleError::Io(e)
    }
}

#[derive(Debug)]
pub enum WaitError {
    /// The file existed at some point during the wait and nothing matched before the deadline.
    Timeout {
        waited: Duration,
        file_exists: bool,
    },
    /// The file never appeared during the whole wait.
    Missing,
    Io(io::Error),
}

impl From<io::Error> for WaitError {
    fn from(e: io::Error) -> Self {
        WaitError::Io(e)
    }
}

/// Where a wait starts reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitFrom {
    /// Only lines appended after the call starts (default).
    End,
    /// The whole existing file too.
    Beginning,
    /// A cursor from an earlier read; falls back to 0 if the inode changed or the file shrank.
    At(Cursor),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Oldest first. Up to `n` lines (matching `grep` when given).
    pub lines: Vec<String>,
    /// Lines examined from the end of the file (before `grep`).
    pub lines_scanned: usize,
    /// Bytes read from the end of the file.
    pub bytes_scanned: u64,
    /// True when the whole file was read (no older lines exist).
    pub reached_start_of_file: bool,
    /// Inode and the file size at read time: pass to `console_wait` as `since`.
    pub cursor: Cursor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitMatch {
    pub line: String,
    pub waited: Duration,
}

pub struct ConsoleLog {
    path: PathBuf,
}

impl ConsoleLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ConsoleLog { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Complete lines appended since `from`, oldest first, and the cursor after the last of them.
    /// A cursor for another inode, or one past the end (truncation), starts from 0. An unterminated
    /// last line is not returned: the cursor stays before it, so the next read gets it whole.
    /// `Ok(None)` when the file does not exist. Reads at most [`READ_NEW_CAP`] bytes per call.
    pub async fn read_new_lines(
        &self,
        from: Option<&Cursor>,
    ) -> io::Result<Option<(Vec<String>, Cursor)>> {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = file.metadata().await?;
        let inode = file_id(&meta);
        let size = meta.len();
        let start = match from {
            Some(c) if c.inode == inode && c.offset <= size => c.offset,
            _ => 0,
        };
        let want = (size - start).min(READ_NEW_CAP) as usize;
        let buf = read_at(&mut file, start, want).await?;
        let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else {
            return Ok(Some((
                Vec::new(),
                Cursor {
                    inode,
                    offset: start,
                    path: Some(self.path.display().to_string()),
                },
            )));
        };
        let lines = buf[..last_nl]
            .split(|b| *b == b'\n')
            .map(|raw| {
                let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
                String::from_utf8_lossy(raw).into_owned()
            })
            .collect();
        let next = Cursor {
            inode,
            offset: start + last_nl as u64 + 1,
            path: Some(self.path.display().to_string()),
        };
        Ok(Some((lines, next)))
    }

    /// The current cursor (inode + size), or `None` when the file does not exist.
    pub fn cursor(&self) -> io::Result<Option<Cursor>> {
        match std::fs::metadata(&self.path) {
            Ok(m) => Ok(Some(Cursor {
                inode: file_id(&m),
                offset: m.len(),
                path: Some(self.path.display().to_string()),
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The last `n` lines (optionally only those matching `grep`), read backwards from the end.
    /// Blocking file I/O runs on the blocking pool.
    pub async fn tail(&self, n: usize, grep: Option<Regex>) -> Result<Tail, ConsoleError> {
        let path = self.path.clone();
        let mut tail = tokio::task::spawn_blocking(move || {
            tail_blocking(&path, n, grep.as_ref(), TAIL_SCAN_CAP)
        })
        .await
        .map_err(|e| ConsoleError::Io(io::Error::other(e)))??;
        tail.cursor.path = Some(self.path.display().to_string());
        Ok(tail)
    }

    /// Wait until a complete line starting at `from` matches `re`, or `timeout` passes.
    pub async fn wait_for(
        &self,
        re: &Regex,
        timeout: Duration,
        from: WaitFrom,
    ) -> Result<WaitMatch, WaitError> {
        self.wait_for_by(&|line: &str| re.is_match(line), timeout, from)
            .await
    }

    /// [`ConsoleLog::wait_for`] with a predicate over each complete line, called once per line in
    /// order. The predicate may keep state (an ordered multi-line wait); it is never called twice
    /// for one line.
    pub async fn wait_for_by(
        &self,
        matches: &(dyn Fn(&str) -> bool + Sync),
        timeout: Duration,
        from: WaitFrom,
    ) -> Result<WaitMatch, WaitError> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut follower = Follower::open(&self.path, from).await?;
        loop {
            let idle = match follower.poll(matches).await? {
                Step::Match(line) => {
                    return Ok(WaitMatch {
                        line,
                        waited: started.elapsed(),
                    })
                }
                // Data was read: more backlog may be pending; yield and poll again.
                Step::Progress => {
                    tokio::task::yield_now().await;
                    false
                }
                Step::Idle => true,
            };
            let now = Instant::now();
            if now >= deadline {
                if !follower.seen {
                    return Err(WaitError::Missing);
                }
                let file_exists = tokio::fs::try_exists(&self.path).await.unwrap_or(false);
                return Err(WaitError::Timeout {
                    waited: now - started,
                    file_exists,
                });
            }
            if idle {
                tokio::time::sleep(POLL.min(deadline - now)).await;
            }
        }
    }
}

fn tail_blocking(
    path: &Path,
    n: usize,
    grep: Option<&Regex>,
    cap: u64,
) -> Result<Tail, ConsoleError> {
    // One retry: a truncation during the read (UnexpectedEof) re-reads from the new size.
    match tail_once(path, n, grep, cap) {
        Err(ConsoleError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
            tail_once(path, n, grep, cap)
        }
        other => other,
    }
}

fn tail_once(path: &Path, n: usize, grep: Option<&Regex>, cap: u64) -> Result<Tail, ConsoleError> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ConsoleError::Missing),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    let cursor = Cursor {
        inode: file_id(&meta),
        offset: meta.len(),
        path: None,
    };
    let mut pos = meta.len();
    // Bytes [pos..) not yet split into lines (the leftover prefix of the current line).
    let mut rest: Vec<u8> = Vec::new();
    // True while `rest` still ends at EOF and no '\n' has been seen from the right: its last
    // piece is then the file's unterminated final line (if non-empty).
    let mut eof_edge = true;
    let mut out: Vec<String> = Vec::new(); // newest first
    let mut scanned = 0usize;
    let mut bytes = 0u64;
    loop {
        // Split off the complete lines of `rest` (scoped so the borrow of `rest` ends first).
        let (stopped, more_lines, leftover) = {
            let pieces: Vec<&[u8]> = rest.split(|b| *b == b'\n').collect();
            let k = pieces.len() - 1;
            // Pieces 1..=k are each followed by a '\n' (or were, at EOF); piece 0 is the leftover.
            let mut ready: Vec<&[u8]> = Vec::new();
            if k >= 1 {
                if eof_edge {
                    eof_edge = false;
                    if !pieces[k].is_empty() {
                        ready.push(pieces[k]);
                    }
                    ready.extend(pieces[1..k].iter().rev());
                } else {
                    ready.extend(pieces[1..=k].iter().rev());
                }
            }
            let mut ready = ready.into_iter();
            let mut stopped = false;
            for raw in ready.by_ref() {
                accept(raw, grep, &mut out, &mut scanned);
                if out.len() >= n {
                    stopped = true;
                    break;
                }
            }
            (stopped, ready.len(), pieces[0].to_vec())
        };
        if stopped {
            // Full only when nothing older is left: no more pieces, no bytes before, no line.
            let leftover_is_line = !eof_edge || !leftover.is_empty();
            let reached = pos == 0 && more_lines == 0 && !leftover_is_line;
            return Ok(finish(out, scanned, bytes, reached, cursor));
        }
        rest = leftover;
        if pos == 0 {
            if !eof_edge || !rest.is_empty() {
                accept(&rest, grep, &mut out, &mut scanned);
            }
            if file.metadata()?.len() < cursor.offset {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "console.log truncated during the read",
                )
                .into());
            }
            return Ok(finish(out, scanned, bytes, true, cursor));
        }
        if bytes >= cap {
            return Ok(finish(out, scanned, bytes, false, cursor));
        }
        let want = CHUNK.max(rest.len() as u64).min(pos).min(cap - bytes);
        pos -= want;
        file.seek(SeekFrom::Start(pos))?;
        let mut buf = vec![0u8; want as usize];
        file.read_exact(&mut buf)?;
        bytes += want;
        buf.extend_from_slice(&rest);
        rest = buf;
    }
}

fn accept(raw: &[u8], grep: Option<&Regex>, out: &mut Vec<String>, scanned: &mut usize) {
    *scanned += 1;
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let line = String::from_utf8_lossy(raw);
    if grep.is_none_or(|re| re.is_match(&line)) {
        out.push(line.into_owned());
    }
}

fn finish(mut out: Vec<String>, scanned: usize, bytes: u64, reached: bool, cursor: Cursor) -> Tail {
    out.reverse();
    Tail {
        lines: out,
        lines_scanned: scanned,
        bytes_scanned: bytes,
        reached_start_of_file: reached,
        cursor,
    }
}

enum Step {
    Match(String),
    /// Bytes were read but no match yet; more may be pending.
    Progress,
    /// Nothing new (or the file is missing).
    Idle,
}

/// The file the follower has open, with its identity.
struct Open {
    file: tokio::fs::File,
    id: u64,
}

/// Follows the path across truncation and rotation. Offsets refer to the open file.
struct Follower {
    path: PathBuf,
    open: Option<Open>,
    offset: u64,
    /// The up to `SIG_LEN` bytes just before `offset`.
    sig: Vec<u8>,
    mtime: Option<SystemTime>,
    /// Bytes of the current incomplete line. Only new bytes are scanned for '\n', so nothing is
    /// re-scanned on later polls.
    partial: Vec<u8>,
    partial_overflow: bool,
    /// The file has existed at some point during this wait.
    seen: bool,
}

impl Follower {
    async fn open(path: &Path, from: WaitFrom) -> io::Result<Self> {
        let mut f = Follower {
            path: path.to_path_buf(),
            open: None,
            offset: 0,
            sig: Vec::new(),
            mtime: None,
            partial: Vec::new(),
            partial_overflow: false,
            seen: false,
        };
        match tokio::fs::File::open(path).await {
            Ok(file) => f.attach(file, from).await?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(f)
    }

    /// Make `file` the followed file, positioned according to `from`.
    async fn attach(&mut self, mut file: tokio::fs::File, from: WaitFrom) -> io::Result<()> {
        let meta = file.metadata().await?;
        let id = file_id(&meta);
        let size = meta.len();
        self.seen = true;
        self.offset = match from {
            WaitFrom::Beginning => 0,
            WaitFrom::End => size,
            WaitFrom::At(c) if c.inode == id && c.offset <= size => c.offset,
            WaitFrom::At(_) => 0,
        };
        self.sig = Vec::new();
        if self.offset > 0 {
            let len = (self.offset as usize).min(SIG_LEN);
            self.sig = read_at(&mut file, self.offset - len as u64, len).await?;
        }
        self.partial.clear();
        self.partial_overflow = false;
        self.mtime = meta.modified().ok();
        self.open = Some(Open { file, id });
        Ok(())
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.sig.clear();
        self.partial.clear();
        self.partial_overflow = false;
    }

    async fn poll(&mut self, matches: &(dyn Fn(&str) -> bool + Sync)) -> io::Result<Step> {
        // 1. Is the path still the file we have open? If not (rotated or removed), drain the old
        //    file to its end first: its last lines were written before the switch.
        if let Some(id) = self.open.as_ref().map(|o| o.id) {
            if path_id(&self.path).await? != Some(id) {
                loop {
                    let (read, hit) = self.read_step(matches).await?;
                    if let Some(line) = hit {
                        return Ok(Step::Match(line));
                    }
                    if read == 0 {
                        break;
                    }
                }
                self.open = None;
                self.reset();
            }
        }
        // 2. No handle: open the path if it exists now (read the new file from its start).
        if self.open.is_none() {
            match tokio::fs::File::open(&self.path).await {
                Ok(file) => self.attach(file, WaitFrom::Beginning).await?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Step::Idle),
                Err(e) => return Err(e),
            }
        }
        // 3. Truncated in place (size shrank, bytes before the offset changed, or mtime went
        //    backwards): start over from 0.
        if self.is_truncated().await? {
            self.reset();
        }
        // 4. One bounded read.
        let (read, hit) = self.read_step(matches).await?;
        if let Some(line) = hit {
            return Ok(Step::Match(line));
        }
        Ok(if read > 0 { Step::Progress } else { Step::Idle })
    }

    async fn is_truncated(&mut self) -> io::Result<bool> {
        let Some(open) = self.open.as_mut() else {
            return Ok(false);
        };
        let meta = open.file.metadata().await?;
        let now_mtime = meta.modified().ok();
        let mut truncated = meta.len() < self.offset;
        if let (Some(prev), Some(now)) = (self.mtime, now_mtime) {
            truncated |= now < prev;
        }
        self.mtime = now_mtime;
        if !truncated && !self.sig.is_empty() {
            let at = self.offset - self.sig.len() as u64;
            truncated = match read_at(&mut open.file, at, self.sig.len()).await {
                Ok(bytes) => bytes != self.sig,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => true,
                Err(e) => return Err(e),
            };
        }
        Ok(truncated)
    }

    /// Read at most `READ_CAP` new bytes from the open file and feed them. Returns the number of
    /// bytes read and a matching line, if one completed.
    async fn read_step(
        &mut self,
        matches: &(dyn Fn(&str) -> bool + Sync),
    ) -> io::Result<(usize, Option<String>)> {
        let Some(open) = self.open.as_mut() else {
            return Ok((0, None));
        };
        let size = open.file.metadata().await?.len();
        if size <= self.offset {
            return Ok((0, None));
        }
        let want = (size - self.offset).min(READ_CAP) as usize;
        let buf = match read_at(&mut open.file, self.offset, want).await {
            Ok(b) => b,
            // Shrank since the metadata call: the next poll sees the truncation.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok((0, None)),
            Err(e) => return Err(e),
        };
        self.offset += buf.len() as u64;
        let mut combined = std::mem::take(&mut self.sig);
        combined.extend_from_slice(&buf);
        self.sig = combined.split_off(combined.len().saturating_sub(SIG_LEN));
        let hit = self.feed(&buf, matches);
        Ok((buf.len(), hit))
    }

    /// Feed new bytes; returns the first complete line that matches.
    fn feed(&mut self, mut data: &[u8], matches: &(dyn Fn(&str) -> bool + Sync)) -> Option<String> {
        while let Some(i) = data.iter().position(|&b| b == b'\n') {
            self.push_partial(&data[..i]);
            let line = self.take_line();
            if matches(&line) {
                return Some(line);
            }
            data = &data[i + 1..];
        }
        self.push_partial(data);
        None
    }

    fn push_partial(&mut self, bytes: &[u8]) {
        let room = PARTIAL_CAP - self.partial.len();
        if bytes.len() <= room {
            self.partial.extend_from_slice(bytes);
        } else {
            self.partial.extend_from_slice(&bytes[..room]);
            self.partial_overflow = true;
        }
    }

    /// Complete the current line (strip a trailing '\r', mark an overflowing line).
    fn take_line(&mut self) -> String {
        let overflow = std::mem::take(&mut self.partial_overflow);
        let mut raw = std::mem::take(&mut self.partial);
        if !overflow && raw.last() == Some(&b'\r') {
            raw.pop();
        }
        let mut line = String::from_utf8_lossy(&raw).into_owned();
        if overflow {
            line.push_str(TRUNCATED_MARK);
        }
        line
    }
}

async fn read_at(file: &mut tokio::fs::File, at: u64, len: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at)).await?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn path_id(path: &Path) -> io::Result<Option<u64>> {
    match tokio::fs::metadata(path).await {
        Ok(m) => Ok(Some(file_id(&m))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

/// No inode numbers elsewhere: rotation is then only detected through the path identity check
/// (always equal), and truncation through size and signature.
#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique temp file, removed on drop.
    struct TempLog(PathBuf);

    impl TempLog {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            TempLog(std::env::temp_dir().join(format!(
                "cs2mcp-log-{tag}-{}-{nanos}.log",
                std::process::id()
            )))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn re(p: &str) -> Regex {
        Regex::new(p).unwrap()
    }

    async fn tail(path: &Path, n: usize, grep: Option<&str>) -> Tail {
        ConsoleLog::new(path).tail(n, grep.map(re)).await.unwrap()
    }

    /// Appends `text` to `path` after `delay`, from a concurrent task.
    fn append_later(path: PathBuf, delay: Duration, text: &'static str) {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .unwrap();
            f.write_all(text.as_bytes()).unwrap();
        });
    }

    #[tokio::test]
    async fn tail_small_file_reports_real_values() {
        let t = TempLog::new("small");
        std::fs::write(t.path(), "a\nb\nc\n").unwrap();
        let got = tail(t.path(), 2, None).await;
        assert_eq!(got.lines, ["b", "c"]);
        assert!(!got.reached_start_of_file);
        assert_eq!(got.lines_scanned, 2);
        let all = tail(t.path(), 10, None).await;
        assert_eq!(all.lines, ["a", "b", "c"]);
        assert!(all.reached_start_of_file);
        assert_eq!(all.lines_scanned, 3);
        assert_eq!(all.bytes_scanned, 6);
        assert_eq!(all.cursor.offset, 6);
    }

    #[tokio::test]
    async fn tail_keeps_unterminated_final_line_and_crlf() {
        let t = TempLog::new("partial");
        std::fs::write(t.path(), "a\r\nb\r\nlast-partial").unwrap();
        let got = tail(t.path(), 5, None).await;
        assert_eq!(got.lines, ["a", "b", "last-partial"]);
        assert!(got.reached_start_of_file);
    }

    #[tokio::test]
    async fn tail_empty_and_newline_only_files() {
        let t = TempLog::new("empty");
        std::fs::write(t.path(), "").unwrap();
        let empty = tail(t.path(), 5, None).await;
        assert!(empty.lines.is_empty());
        assert!(empty.reached_start_of_file);
        std::fs::write(t.path(), "\n").unwrap();
        assert_eq!(tail(t.path(), 5, None).await.lines, [""]);
    }

    #[tokio::test]
    async fn tail_large_file_reads_from_end() {
        let t = TempLog::new("large");
        let mut body = String::new();
        for i in 0..20_000 {
            body.push_str(&format!(
                "line {i:06} some padding to make it a little longer\n"
            ));
        }
        assert!(body.len() > 1_000_000);
        std::fs::write(t.path(), &body).unwrap();
        let got = tail(t.path(), 5, None).await;
        assert_eq!(got.lines.len(), 5);
        assert!(got.lines[4].starts_with("line 019999 "));
        assert!(got.lines[0].starts_with("line 019995 "));
        assert!(!got.reached_start_of_file);
        assert!(got.bytes_scanned < body.len() as u64);
        let many = tail(t.path(), 2000, None).await;
        assert_eq!(many.lines.len(), 2000);
        assert!(many.lines[0].starts_with("line 018000 "));
    }

    #[tokio::test]
    async fn tail_line_longer_than_a_chunk() {
        let t = TempLog::new("longline");
        let long = "x".repeat(200_000);
        std::fs::write(t.path(), format!("{long}\nend\n")).unwrap();
        let got = tail(t.path(), 2, None).await;
        assert_eq!(got.lines.len(), 2);
        assert_eq!(got.lines[0], long);
        assert_eq!(got.lines[1], "end");
    }

    #[tokio::test]
    async fn tail_crlf_split_across_64k_chunk_boundary() {
        let t = TempLog::new("crlf-chunk");
        // The first backward read covers the last 64 KiB and starts at the '\n' of the "\r\n";
        // the '\r' is therefore read by the next step.
        let mut body = format!("{}\r\n", "A".repeat(100));
        body.push_str(&"x".repeat(65534));
        body.push('\n');
        assert_eq!(body.len(), 65637);
        std::fs::write(t.path(), &body).unwrap();
        let got = tail(t.path(), 2, None).await;
        assert_eq!(got.lines[0], "A".repeat(100));
        assert_eq!(got.lines[1], "x".repeat(65534));
        assert!(got.reached_start_of_file);
    }

    #[tokio::test]
    async fn tail_multibyte_char_split_across_64k_chunk_boundary() {
        let t = TempLog::new("utf8-chunk");
        // 'é' is C3 A9; the first backward read starts at A9, so C3 is in the next read.
        let body = format!("é{}\n", "y".repeat(65534));
        assert_eq!(body.len(), 65537);
        std::fs::write(t.path(), &body).unwrap();
        let got = tail(t.path(), 1, None).await;
        assert_eq!(got.lines, [format!("é{}", "y".repeat(65534))]);
        assert!(!got.lines[0].contains('\u{FFFD}'));
    }

    #[tokio::test]
    async fn tail_grep_filters_before_counting() {
        let t = TempLog::new("grep");
        let mut body = String::new();
        for i in 0..1000 {
            let tag = if i % 100 == 0 { "ERR" } else { "ok " };
            body.push_str(&format!("{tag} {i}\n"));
        }
        std::fs::write(t.path(), body).unwrap();
        let got = tail(t.path(), 3, Some("ERR")).await;
        assert_eq!(got.lines, ["ERR 700", "ERR 800", "ERR 900"]);
        assert!(!got.reached_start_of_file);
        let none = tail(t.path(), 3, Some("NOPE")).await;
        assert!(none.lines.is_empty());
        assert!(none.reached_start_of_file);
    }

    #[tokio::test]
    async fn tail_scan_cap_stops_and_reports_bytes() {
        let t = TempLog::new("cap");
        let body = "0123456789abcdef\n".repeat(65536); // 1 MiB
        std::fs::write(t.path(), &body).unwrap();
        let cap = 256 * 1024;
        let got = tail_blocking(t.path(), 1_000_000, None, cap).unwrap();
        assert_eq!(got.bytes_scanned, cap);
        assert!(!got.reached_start_of_file);
        assert!(got.lines_scanned > 0);
        // Uncapped, the same file is read in full.
        let full = tail_blocking(t.path(), 1_000_000, None, u64::MAX).unwrap();
        assert_eq!(full.bytes_scanned, body.len() as u64);
        assert!(full.reached_start_of_file);
        assert_eq!(full.lines_scanned, 65536);
    }

    #[tokio::test]
    async fn tail_tolerates_invalid_utf8() {
        let t = TempLog::new("utf8");
        std::fs::write(t.path(), b"fine\nbad \xff\xfe bytes\n").unwrap();
        let got = tail(t.path(), 2, None).await;
        assert_eq!(got.lines.len(), 2);
        assert_eq!(got.lines[0], "fine");
        assert_eq!(got.lines[1], "bad \u{FFFD}\u{FFFD} bytes");
    }

    #[tokio::test]
    async fn tail_missing_file_is_typed() {
        let t = TempLog::new("missing");
        let err = ConsoleLog::new(t.path()).tail(5, None).await.unwrap_err();
        assert!(matches!(err, ConsoleError::Missing));
        assert!(ConsoleLog::new(t.path()).cursor().unwrap().is_none());
    }

    #[tokio::test]
    async fn cursor_matches_tail_cursor() {
        let t = TempLog::new("cursor");
        std::fs::write(t.path(), "one\ntwo\n").unwrap();
        let log = ConsoleLog::new(t.path());
        let c = log.cursor().unwrap().unwrap();
        assert_eq!(c.offset, 8);
        assert_eq!(log.tail(1, None).await.unwrap().cursor, c);
    }

    #[tokio::test]
    async fn wait_matches_line_appended_after_start() {
        let t = TempLog::new("wait-append");
        std::fs::write(t.path(), "boot\n").unwrap();
        append_later(
            t.path().to_path_buf(),
            Duration::from_millis(100),
            "READY now\n",
        );
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("READY"), Duration::from_secs(5), WaitFrom::End)
            .await
            .unwrap();
        assert_eq!(m.line, "READY now");
        assert!(m.waited >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn wait_ignores_preexisting_matches() {
        let t = TempLog::new("wait-old");
        std::fs::write(t.path(), "READY old\n").unwrap();
        let err = ConsoleLog::new(t.path())
            .wait_for(&re("READY"), Duration::from_millis(200), WaitFrom::End)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            WaitError::Timeout {
                file_exists: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn wait_from_beginning_searches_existing_content() {
        let t = TempLog::new("wait-start");
        std::fs::write(t.path(), "junk\nREADY old\n").unwrap();
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("READY"), Duration::from_secs(5), WaitFrom::Beginning)
            .await
            .unwrap();
        assert_eq!(m.line, "READY old");
    }

    #[tokio::test]
    async fn wait_from_cursor_matches_only_after_it() {
        let t = TempLog::new("wait-since");
        std::fs::write(t.path(), "MATCH before\n").unwrap();
        let log = ConsoleLog::new(t.path());
        let since = log.cursor().unwrap().unwrap();
        append_later(
            t.path().to_path_buf(),
            Duration::from_millis(100),
            "MATCH after\n",
        );
        let m = log
            .wait_for(&re("MATCH"), Duration::from_secs(5), WaitFrom::At(since))
            .await
            .unwrap();
        assert_eq!(m.line, "MATCH after");
    }

    #[tokio::test]
    async fn wait_from_cursor_falls_back_to_zero_on_other_inode_or_shrink() {
        let t = TempLog::new("wait-since-fallback");
        std::fs::write(t.path(), "MATCH existing\n").unwrap();
        let log = ConsoleLog::new(t.path());
        let real = log.cursor().unwrap().unwrap();
        let other_inode = Cursor {
            inode: real.inode.wrapping_add(1),
            offset: real.offset,
            path: None,
        };
        let past_end = Cursor {
            inode: real.inode,
            offset: real.offset + 1000,
            path: None,
        };
        for since in [other_inode, past_end] {
            let m = log
                .wait_for(&re("MATCH"), Duration::from_secs(2), WaitFrom::At(since))
                .await
                .unwrap();
            assert_eq!(m.line, "MATCH existing");
        }
    }

    #[tokio::test]
    async fn wait_times_out() {
        let t = TempLog::new("wait-timeout");
        std::fs::write(t.path(), "").unwrap();
        let started = Instant::now();
        let err = ConsoleLog::new(t.path())
            .wait_for(&re("never"), Duration::from_millis(150), WaitFrom::End)
            .await
            .unwrap_err();
        match err {
            WaitError::Timeout { waited, .. } => assert!(waited >= Duration::from_millis(150)),
            other => panic!("expected timeout, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn wait_missing_for_whole_wait_is_missing_not_timeout() {
        let t = TempLog::new("wait-never-there");
        let err = ConsoleLog::new(t.path())
            .wait_for(&re("x"), Duration::from_millis(150), WaitFrom::End)
            .await
            .unwrap_err();
        assert!(matches!(err, WaitError::Missing));
    }

    #[tokio::test]
    async fn wait_waits_for_missing_file_to_appear() {
        let t = TempLog::new("wait-create");
        append_later(
            t.path().to_path_buf(),
            Duration::from_millis(100),
            "ARRIVED\n",
        );
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("ARRIVED"), Duration::from_secs(5), WaitFrom::End)
            .await
            .unwrap();
        assert_eq!(m.line, "ARRIVED");
    }

    #[tokio::test]
    async fn wait_survives_truncation_mid_wait() {
        let t = TempLog::new("wait-trunc");
        // Long enough that the follower's offset is well past the new content's length.
        std::fs::write(t.path(), format!("{}\n", "p".repeat(60))).unwrap();
        let path = t.path().to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(&path, "NEW match\n").unwrap(); // -conclearlog then new output
        });
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("match"), Duration::from_secs(5), WaitFrom::End)
            .await
            .unwrap();
        assert_eq!(m.line, "NEW match");
    }

    #[tokio::test]
    async fn truncate_then_regrow_past_old_offset_is_detected() {
        let t = TempLog::new("regrow");
        std::fs::write(t.path(), format!("old\n{}", "p".repeat(96))).unwrap(); // 100 bytes
        let mut f = Follower::open(t.path(), WaitFrom::End).await.unwrap();
        assert_eq!(f.offset, 100);
        // Truncate in place (same inode) and write MORE than the old offset before the next poll,
        // with the match in the first part.
        std::fs::write(t.path(), format!("MATCH first\n{}", "z".repeat(290))).unwrap();
        match f.poll(&|l: &str| re("MATCH").is_match(l)).await.unwrap() {
            Step::Match(line) => assert_eq!(line, "MATCH first"),
            _ => panic!("truncate-and-regrow was not detected"),
        }
    }

    #[tokio::test]
    async fn rotation_drains_old_file_before_switching() {
        let t = TempLog::new("drain");
        std::fs::write(t.path(), "x\n").unwrap();
        let mut f = Follower::open(t.path(), WaitFrom::End).await.unwrap();
        // The old file gets a match after our last poll, then is rotated away before the next.
        {
            use std::io::Write;
            let mut old = std::fs::OpenOptions::new()
                .append(true)
                .open(t.path())
                .unwrap();
            old.write_all(b"OLD MATCH\n").unwrap();
        }
        let staged = t.path().with_extension("staged");
        std::fs::write(&staged, "NEW MATCH\n").unwrap();
        std::fs::rename(&staged, t.path()).unwrap();
        let first = f.poll(&|l: &str| re("MATCH").is_match(l)).await.unwrap();
        assert!(matches!(first, Step::Match(ref l) if l == "OLD MATCH"));
        let second = f.poll(&|l: &str| re("MATCH").is_match(l)).await.unwrap();
        assert!(matches!(second, Step::Match(ref l) if l == "NEW MATCH"));
    }

    /// Writes `prefix` then a 256 KiB boundary inside the second read, and returns the content.
    fn read_boundary_content(tail_at_boundary: &str) -> String {
        let mut body = "A".repeat(256 * 1024 - 1);
        body.push_str(tail_at_boundary);
        body
    }

    #[tokio::test]
    async fn follower_crlf_split_across_read_boundary() {
        let t = TempLog::new("follow-crlf");
        // The first 256 KiB read ends with '\r'; the '\n' starts the second read.
        std::fs::write(t.path(), read_boundary_content("\r\n")).unwrap();
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("A"), Duration::from_secs(5), WaitFrom::Beginning)
            .await
            .unwrap();
        assert_eq!(m.line, "A".repeat(256 * 1024 - 1));
    }

    #[tokio::test]
    async fn follower_multibyte_split_across_read_boundary() {
        let t = TempLog::new("follow-utf8");
        // 'é' (C3 A9) straddles the 256 KiB read boundary.
        std::fs::write(t.path(), read_boundary_content("é\n")).unwrap();
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("A"), Duration::from_secs(5), WaitFrom::Beginning)
            .await
            .unwrap();
        assert_eq!(m.line, format!("{}é", "A".repeat(256 * 1024 - 1)));
    }

    #[tokio::test]
    async fn overlong_line_is_capped_and_marked() {
        let t = TempLog::new("overlong");
        let body = format!("{}\n", "B".repeat(PARTIAL_CAP + 50_000));
        std::fs::write(t.path(), body).unwrap();
        let m = ConsoleLog::new(t.path())
            .wait_for(
                &re(r"\[truncated\]$"),
                Duration::from_secs(5),
                WaitFrom::Beginning,
            )
            .await
            .unwrap();
        assert!(m.line.ends_with(TRUNCATED_MARK));
        assert_eq!(m.line.len(), PARTIAL_CAP + TRUNCATED_MARK.len());
    }

    #[tokio::test]
    async fn wait_ignores_partial_line_until_newline() {
        let t = TempLog::new("wait-partial");
        std::fs::write(t.path(), "").unwrap();
        let path = t.path().to_path_buf();
        tokio::spawn(async move {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            f.write_all(b"MATCH").unwrap();
            f.flush().unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            f.write_all(b"\n").unwrap();
        });
        let started = Instant::now();
        let m = ConsoleLog::new(t.path())
            .wait_for(&re("MATCH"), Duration::from_secs(5), WaitFrom::End)
            .await
            .unwrap();
        assert_eq!(m.line, "MATCH");
        // Completed at the newline (~350 ms), not at the first bytes (~50 ms).
        assert!(started.elapsed() >= Duration::from_millis(300));
    }
}
