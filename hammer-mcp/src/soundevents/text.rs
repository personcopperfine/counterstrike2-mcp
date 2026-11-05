//! Comment-preserving edits of a KV3 `.vsndevts` text (WP-S2).
//!
//! `cs2_kv3::parse` drops comments (the crate's documented limitation — the addon template is
//! one of the two shipped files that do not round-trip because of it), so an edit that re-serialises
//! the whole document would destroy the user's comments. This module instead edits the **raw
//! text**: a one-pass, depth-1 scanner finds each top-level event span (`"name" = { … }`),
//! and every [`SpliceOp`] replaces/cuts/renames exactly one span. All text outside the touched
//! spans is copied byte-for-byte, so comments, blank lines and the file's own CRLF layout
//! survive every edit. A comment **inside** a replaced span is unavoidably lost — the count of
//! the lost comment lines is reported per event ([`Spliced::comments_dropped`]), never silently.
//!
//! Safety net: the caller (the `edit_soundevents` tool) has already parsed the file with
//! `cs2_kv3::parse` and computed the intended root before splicing; it re-parses the spliced
//! text and refuses to write anything when the two disagree ([`SpliceError`] and every
//! mismatch are `InternalError`s, never a write). The scanner is a single pass, no recursion,
//! and understands the same lexemes the cs2-kv3 lexer does: `"…"` strings with escapes, `"""`
//! raw strings, `//` and `/* */` comments, `#[ … ]` blobs. The scan is **byte-oriented**, so a
//! multi-byte character in a comment or in a string is never a char-boundary trap.

use cs2_kv3::{Object, Value};

/// One pre-validated splice edit. The tool computes these against the parsed document (merges
/// resolved, existence checked), so `splice` never invents behaviour: an op naming an event the
/// scanner cannot find is an error, not a silent no-op.
#[derive(Debug, Clone, PartialEq)]
pub enum SpliceOp {
    /// Write `object` as the event `name`: the span's text is replaced when the event exists in
    /// the current text, otherwise the event is inserted before the root's closing `}` (a
    /// `set` on a new name, `createFile` included).
    Set { name: String, object: Object },
    /// Cut the event's span plus its trailing line ending.
    Remove { name: String },
    /// Rewrite the event's key token, then exactly the `base` / `soundevent_01` string tokens of
    /// the events that reference `from` — the set the tool has already rewritten at the document
    /// level (`soundevents_edit::rewrite_references`). A token that merely holds the same text
    /// somewhere else (another property such as a `vsnd_files_track_01` entry or a `description`,
    /// or a comment) is left alone: the document side leaves it too, and rewriting it would make
    /// the caller's round-trip check refuse the edit (review M2).
    Rename { from: String, to: String },
}

/// The spliced text plus what the splice consumed.
#[derive(Debug, Clone, PartialEq)]
pub struct Spliced {
    pub text: String,
    /// Per touched event, how many comment lines inside its replaced span were lost.
    /// `remove`/`rename` never drop comments (only a `set` replaces a span's body).
    pub comments_dropped: Vec<(String, usize)>,
}

/// A splice the scanner could not perform. The tool maps this to `InternalError` and writes
/// nothing — a splice that cannot understand the text must never guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpliceError(pub String);

impl std::fmt::Display for SpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Apply `ops` in order, each against the text the previous ones produced (re-scanned every
/// time — event files are small, and a fresh scan cannot drift).
pub fn splice(text: &str, ops: &[SpliceOp]) -> Result<Spliced, SpliceError> {
    let mut current = text.to_string();
    let mut dropped: Vec<(String, usize)> = Vec::new();
    for op in ops {
        let scan = scan(&current)?;
        match op {
            SpliceOp::Set { name, object } => match scan.event(name) {
                Some(event) => {
                    let rendered = render_event(name, object, event.key_quoted, scan.eol);
                    current = replace_range(
                        &current,
                        event.line_start..event.span_end,
                        &format!("{}{rendered}", event.indent),
                    )
                    .text;
                    if event.comment_lines > 0 {
                        dropped.push((name.clone(), event.comment_lines));
                    }
                }
                None => {
                    // New event: inserted before the root's closing `}` (a `set` of a new name),
                    // always with a quoted key — the addon template's style for event names.
                    let rendered = format!(
                        "\t{}{}",
                        render_event(name, object, true, scan.eol),
                        scan.eol
                    );
                    current =
                        insert_before_root_close(&current, &scan, &rendered, scan.eol)?.text;
                }
            },
            SpliceOp::Remove { name } => {
                let Some(event) = scan.event(name) else {
                    return Err(SpliceError(format!(
                        "event {name:?} to remove was not found in the text"
                    )));
                };
                // The event's whole line (indent included) plus its trailing line ending, so no
                // blank line or stray indent is left behind.
                let mut end = event.span_end;
                if current[end..].starts_with(scan.eol) {
                    end += scan.eol.len();
                }
                current = replace_range(&current, event.line_start..end, "").text;
            }
            SpliceOp::Rename { from, to } => {
                let Some(event) = scan.event(from) else {
                    return Err(SpliceError(format!(
                        "event {from:?} to rename was not found in the text"
                    )));
                };
                // The key token first, then only the `base` / `soundevent_01` string tokens that
                // hold the old name: the exact set the document side rewrote. Every other token
                // that happens to carry the same text (a `vsnd_files_track_01` path, a
                // `description`, a comment) stays byte-for-byte, or the caller's round-trip check
                // would refuse the edit (review M2).
                let from_token = format!("\"{from}\"");
                let to_token = format!("\"{to}\"");
                let raw_to_token = format!("\"\"\"{to}\"\"\"");
                let mut edits: Vec<(usize, usize, String)> =
                    vec![(event.key_start, event.key_end, key_repr(to, event.key_quoted))];
                for candidate in &scan.events {
                    for range in &candidate.refs {
                        let token = &current[range.clone()];
                        if token == from_token.as_str() {
                            edits.push((range.start, range.end, to_token.clone()));
                        } else if raw_token_holds(token, from) {
                            edits.push((range.start, range.end, raw_to_token.clone()));
                        }
                    }
                }
                // Right to left, so the offsets of the edits still to come stay valid.
                edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
                let mut text = current.clone();
                for (start, end, with) in edits {
                    text = replace_range(&text, start..end, &with).text;
                }
                current = text;
            }
        }
    }
    Ok(Spliced { text: current, comments_dropped: dropped })
}

// -------------------------------------------------------------------------------------------
// The scanner
// -------------------------------------------------------------------------------------------

/// One top-level event found in the text: `"name" = { … }` (byte offsets into the scanned text).
#[derive(Debug, Clone, PartialEq)]
struct EventSpan {
    name: String,
    /// Start of the event's line (its leading whitespace), or of the key token when the key does
    /// not start a line. `set`/`remove` operate from here, so the indent never leaks.
    line_start: usize,
    /// The line's leading whitespace when the key starts its own line (re-emitted by `set`).
    indent: String,
    /// Start of the key token (the opening quote or the identifier's first byte).
    key_start: usize,
    /// End of the key token (one past the closing quote / identifier).
    key_end: usize,
    /// Whether the key was quoted in the source (re-emitted in the same style).
    key_quoted: bool,
    /// One past the event object's closing `}`.
    span_end: usize,
    /// Comment lines inside the event's object (what a `set` would lose).
    comment_lines: usize,
    /// The byte ranges of the event's own `base` / `soundevent_01` string tokens — the
    /// references a [`SpliceOp::Rename`] rewrites, and nothing else.
    refs: Vec<std::ops::Range<usize>>,
}

/// What one pass over the raw text found.
#[derive(Debug, Clone)]
struct Scan {
    eol: &'static str,
    events: Vec<EventSpan>,
    /// Offset of the root object's closing `}`.
    root_close: usize,
}

impl Scan {
    fn event(&self, name: &str) -> Option<&EventSpan> {
        self.events.iter().find(|event| event.name == name)
    }
}

/// Scan the file: header line, then every top-level member whose value is an object is an event.
/// The caller has already parsed the text with `cs2_kv3::parse`, so the structure is known
/// well-formed; anything the scanner still cannot make sense of is an error (the tool refuses
/// rather than splices blindly).
fn scan(text: &str) -> Result<Scan, SpliceError> {
    let eol: &'static str = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut c = Cursor { text, b: text.as_bytes(), i: 0 };
    // The `<!-- kv3 … -->` header line; `cs2_kv3::parse` has already required it.
    let header_end = c
        .text
        .find('\n')
        .map(|at| at + 1)
        .ok_or_else(|| SpliceError("no line ending after the kv3 header".into()))?;
    c.i = header_end;
    c.skip_trivia();
    if c.peek() != Some(b'{') {
        return Err(SpliceError(format!(
            "expected the root object's `{{` at byte {}, found {:?}",
            c.i,
            c.rest_first()
        )));
    }
    c.i += 1;
    let mut events = Vec::new();
    let root_close;
    loop {
        c.skip_trivia();
        let Some(byte) = c.peek() else {
            return Err(SpliceError("the root object is never closed".into()));
        };
        if byte == b'}' {
            root_close = c.i;
            break;
        }
        // A member key: a quoted string or a bare identifier. When the key starts its own line
        // (the Valve layout), the span owns that line from its first indent byte.
        let (line_start, indent) = {
            let before = c.b.get(..c.i).unwrap_or(c.b);
            let after_newline = before
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|at| at + 1)
                .unwrap_or(0);
            let gap = c.b.get(after_newline..c.i).unwrap_or(&[]);
            let own_line = !gap.is_empty() && gap.iter().all(|byte| *byte == b'\t' || *byte == b' ');
            if own_line {
                (after_newline, c.text.get(after_newline..c.i).unwrap_or("").to_string())
            } else {
                (c.i, String::new())
            }
        };
        let key_start = c.i;
        let (name, key_quoted) = c.read_key()?;
        let key_end = c.i;
        c.skip_trivia();
        if c.peek() != Some(b'=') {
            return Err(SpliceError(format!(
                "expected `=` after the key {name:?} at byte {}",
                c.i
            )));
        }
        c.i += 1;
        c.skip_trivia();
        let mut comment_lines = 0;
        if c.peek() == Some(b'{') {
            // An event: the object's span is the edit unit.
            let mut refs = Vec::new();
            c.skip_event_object(&mut comment_lines, &mut refs)?;
            events.push(EventSpan {
                name,
                line_start,
                indent,
                key_start,
                key_end,
                key_quoted,
                span_end: c.i,
                comment_lines,
                refs,
            });
        } else {
            // Another member (`version = …`): skipped whole, never touched.
            c.skip_value(&mut comment_lines)?;
        }
        c.skip_trivia();
        if c.peek() == Some(b',') {
            c.i += 1;
        }
    }
    Ok(Scan { eol, events, root_close })
}

/// Byte-cursor over the raw text.
struct Cursor<'a> {
    text: &'a str,
    b: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn rest_first(&self) -> String {
        self.text.get(self.i..).unwrap_or("").chars().take(12).collect()
    }

    /// Whitespace, `//` line comments and `/* */` block comments.
    fn skip_trivia(&mut self) {
        self.skip_trivia_counting(None);
    }

    /// [`Cursor::skip_trivia`], but when `comments` is given every consumed comment **line** is
    /// added to it (`set` on the owning event loses them — review L4: a run of `//` lines counts
    /// one per line, a `/* */` one per line it covers).
    ///
    /// Every step is a **byte** step: a multi-byte character inside a comment must never make the
    /// cursor slice the `str` at a byte that is not a char boundary (review H1).
    fn skip_trivia_counting(&mut self, mut comments: Option<&mut usize>) {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => self.i += 1,
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'/') => {
                    self.i += 2;
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break; // the line ending itself is whitespace, next round
                        }
                        self.i += 1;
                    }
                    if let Some(count) = comments.as_deref_mut() {
                        *count += 1;
                    }
                }
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'*') => {
                    let start = self.i;
                    self.i += 2;
                    loop {
                        if self.i >= self.b.len() {
                            break; // unterminated: `parse` would already have refused
                        }
                        if self.b[self.i..].starts_with(b"*/") {
                            self.i += 2;
                            break;
                        }
                        self.i += 1;
                    }
                    if let Some(count) = comments.as_deref_mut() {
                        *count += 1 + self.text[start..self.i].matches('\n').count();
                    }
                }
                _ => return,
            }
        }
    }

    /// At `"`: skip a `"…"` string (with `\\` escapes) or a `"""…"""` raw string.
    fn skip_string(&mut self) -> Result<(), SpliceError> {
        let raw = self.text[self.i..].starts_with("\"\"\"");
        let at = self.i;
        self.i += if raw { 3 } else { 1 };
        loop {
            match self.peek() {
                None => {
                    return Err(SpliceError(format!("unterminated string at byte {at}")));
                }
                Some(b'"') => {
                    if raw {
                        if self.text[self.i..].starts_with("\"\"\"") {
                            self.i += 3;
                            return Ok(());
                        }
                        self.i += 1;
                    } else {
                        self.i += 1;
                        return Ok(());
                    }
                }
                Some(b'\\') if !raw => self.i += 2,
                _ => self.i += 1,
            }
        }
    }

    /// At `{` or `[`: skip the whole nested value, counting the comment lines it contains.
    fn skip_nested(&mut self, comments: &mut usize) -> Result<(), SpliceError> {
        let close = match self.peek() {
            Some(b'{') => b'}',
            Some(b'[') => b']',
            other => {
                return Err(SpliceError(format!(
                    "expected `{{` or `[` at byte {}, found {other:?}",
                    self.i
                )))
            }
        };
        let mut depth = 0u32;
        loop {
            match self.peek() {
                None => return Err(SpliceError("unterminated block value".into())),
                Some(b'"') => self.skip_string()?,
                Some(b'/') if matches!(self.b.get(self.i + 1), Some(b'/') | Some(b'*')) => {
                    // Every comment line of the run, one per line (review L4).
                    self.skip_trivia_counting(Some(comments));
                }
                Some(c) if c == b'{' || c == b'[' => {
                    depth += 1;
                    self.i += 1;
                }
                Some(c) if c == b'}' || c == b']' => {
                    depth = depth.saturating_sub(1);
                    self.i += 1;
                    if depth == 0 {
                        if c != close {
                            return Err(SpliceError(format!(
                                "block opened at byte {} closed by the wrong bracket",
                                self.i
                            )));
                        }
                        return Ok(());
                    }
                }
                _ => self.i += 1,
            }
        }
    }

    /// At an event object's `{`: walk its depth-1 members (`key = value`; a key is never a flag —
    /// `flag:value` is a *value* form), count every comment line inside it, and record the byte
    /// range of every string token that is the value of a `base` or `soundevent_01` member.
    ///
    /// Those are exactly the references a [`SpliceOp::Rename`] rewrites: every other member is
    /// skipped whole, so a `vsnd_files_track_01` entry or a `description` that happens to hold the
    /// same text keeps it (the document side rewrites only `base` / `soundevent_01`).
    fn skip_event_object(
        &mut self,
        comments: &mut usize,
        refs: &mut Vec<std::ops::Range<usize>>,
    ) -> Result<(), SpliceError> {
        if self.peek() != Some(b'{') {
            return Err(SpliceError(format!(
                "expected the event's `{{` at byte {}, found {:?}",
                self.i,
                self.peek()
            )));
        }
        self.i += 1;
        loop {
            self.skip_trivia_counting(Some(&mut *comments));
            match self.peek() {
                None => return Err(SpliceError("an event object is never closed".into())),
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => {}
            }
            let (key, _quoted) = self.read_key()?;
            self.skip_trivia_counting(Some(&mut *comments));
            if self.peek() != Some(b'=') {
                return Err(SpliceError(format!(
                    "expected `=` after the key {key:?} at byte {}",
                    self.i
                )));
            }
            self.i += 1;
            self.skip_trivia_counting(Some(&mut *comments));
            if key == "base" || key == "soundevent_01" {
                self.collect_reference_value(&key, comments, refs)?;
            } else {
                self.skip_value(comments)?;
            }
            self.skip_trivia_counting(Some(&mut *comments));
            if self.peek() == Some(b',') {
                self.i += 1;
            }
        }
    }

    /// At a `base` / `soundevent_01` value: record the range of every string token it holds —
    /// `base` is one string, `soundevent_01` a string or an array of strings (the shapes the
    /// document side's `rewrite_references` walks). Any other shape is skipped like a normal
    /// value, and a nested array's strings are not references there either.
    fn collect_reference_value(
        &mut self,
        key: &str,
        comments: &mut usize,
        refs: &mut Vec<std::ops::Range<usize>>,
    ) -> Result<(), SpliceError> {
        if self.peek() == Some(b'"') {
            let start = self.i;
            self.skip_string()?;
            refs.push(start..self.i);
            return Ok(());
        }
        if key == "soundevent_01" && self.peek() == Some(b'[') {
            self.i += 1;
            loop {
                self.skip_trivia_counting(Some(&mut *comments));
                match self.peek() {
                    None => return Err(SpliceError("unterminated `soundevent_01` array".into())),
                    Some(b']') => {
                        self.i += 1;
                        return Ok(());
                    }
                    Some(b'"') => {
                        let start = self.i;
                        self.skip_string()?;
                        refs.push(start..self.i);
                    }
                    _ => self.skip_value(comments)?,
                }
                self.skip_trivia_counting(Some(&mut *comments));
                if self.peek() == Some(b',') {
                    self.i += 1;
                }
            }
        }
        self.skip_value(comments)
    }

    /// Skip a non-object member value (scalar, array, object, flagged value, blob). `parse` has
    /// already validated the text, so this only has to land past the value.
    fn skip_value(&mut self, comments: &mut usize) -> Result<(), SpliceError> {
        match self.peek() {
            Some(b'"') => self.skip_string(),
            Some(b'[') | Some(b'{') => self.skip_nested(comments),
            Some(b'#') => {
                // `#[ hex bytes ]`
                self.i += 2;
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == b']' {
                        return Ok(());
                    }
                }
                Err(SpliceError("unterminated `#[` blob".into()))
            }
            // A scalar or a flagged value (`flag: value`): read the scalar/flag name until a
            // delimiter, then a flag's `:` — which is **not** consumed by the loop — continues
            // with another value whatever the spacing (`resource:"x"` and `resource: "x"`, review
            // L3: consuming the `:` here left the flagged value unskipped).
            Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' => {
                while let Some(c) = self.peek() {
                    if c.is_ascii_whitespace() || matches!(c, b',' | b'}' | b']' | b':') {
                        break;
                    }
                    self.i += 1;
                }
                self.skip_trivia();
                if self.peek() == Some(b':') {
                    self.i += 1;
                    self.skip_trivia();
                    self.skip_value(comments)
                } else {
                    Ok(())
                }
            }
            other => Err(SpliceError(format!(
                "unexpected value start at byte {}: {other:?}",
                self.i
            ))),
        }
    }

    /// At a member key: a quoted string (the name is the raw text between the quotes — event
    /// names carry no escapes) or a bare identifier.
    fn read_key(&mut self) -> Result<(String, bool), SpliceError> {
        match self.peek() {
            Some(b'"') => {
                // A `"""…"""` raw string has no escapes; a plain `"…"` does (review L5: the test
                // was inverted, so `\\` was skipped for raw keys only).
                let raw = self.text[self.i..].starts_with("\"\"\"");
                self.i += 1;
                let start = self.i;
                while let Some(c) = self.peek() {
                    match c {
                        b'"' => {
                            let name = self.text[start..self.i].to_string();
                            self.i += 1;
                            return Ok((name, true));
                        }
                        b'\\' if !raw => self.i += 2,
                        b'\n' => {
                            return Err(SpliceError(format!(
                                "newline in a quoted key at byte {}",
                                self.i
                            )))
                        }
                        _ => self.i += 1,
                    }
                }
                Err(SpliceError("unterminated quoted key".into()))
            }
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Ok((self.text[start..self.i].to_string(), false))
            }
            other => Err(SpliceError(format!(
                "expected a member key at byte {}, found {other:?}",
                self.i
            ))),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------------------------

/// The round-trip equality the splice caller checks: same member names in the same order, same
/// values. Root-level keys are compared by text only — the splice may re-emit a key in a
/// different quoting style than the source had — while nested values compare fully (`PartialEq`;
/// their keys reach both sides from the same objects, so their flags agree).
pub(crate) fn roots_equal(a: &Object, b: &Object) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|((an, av), (bn, bv))| an == bn && av == bv)
}


/// The event's text from the key token to the closing `}`: `key = ` + the cs2-kv3 writer's block
/// rendering at indent 1, re-lined to the file's own line ending (`write_value` emits `\n`).
/// Everything from the key to `}` is replaced, so the indent byte(s) the file had before the key
/// (outside the span) are kept.
fn render_event(name: &str, object: &Object, key_quoted: bool, eol: &str) -> String {
    let block = cs2_kv3::write_value(&Value::Object(object.clone()), 1);
    format!("{} = {}{}", key_repr(name, key_quoted), eol, block.replace('\n', eol))
}

/// `"name"` or a bare identifier — the style the event's key had in the source (new events are
/// always quoted, the addon template's style). A bare source key stays bare only while the name
/// is a KV3 identifier (`cs2_kv3`'s private `is_identifier`: first byte ASCII alpha or `_`,
/// every byte after ASCII alnum/`_`/`.` — copied here rather than exposing the private helper);
/// any other name is QUOTED even when the source key was bare, or the emitted token would not
/// lex (review N6: a rename of `bare_one` to `1up` used to emit a bare `1up`).
fn key_repr(name: &str, quoted: bool) -> String {
    if quoted || !is_kv3_identifier(name) {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

/// `cs2_kv3`'s key-identifier rule (its private `is_identifier`, `crates/cs2-kv3/src/lib.rs`): a bare
/// key token is `[A-Za-z_][A-Za-z0-9_.]*`.
fn is_kv3_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Does this recorded `"""…"""` raw-string token decode to `name`? The document side compares
/// **decoded** strings, so a raw-string `base` / `soundevent_01` reference is rewritten there and
/// must be rewritten here too (`cs2_kv3`'s raw form collapses CRLF and strips one leading and one
/// trailing line ending). A plain `"…"` token cannot decode to a name it is not spelled as: the
/// lexer's escapes never produce one.
fn raw_token_holds(token: &str, name: &str) -> bool {
    let Some(inner) = token
        .strip_prefix("\"\"\"")
        .and_then(|rest| rest.strip_suffix("\"\"\""))
    else {
        return false;
    };
    let normalised = inner.replace("\r\n", "\n");
    let inner = normalised.strip_prefix('\n').unwrap_or(&normalised);
    inner.strip_suffix('\n').unwrap_or(inner) == name
}

fn replace_range(text: &str, range: std::ops::Range<usize>, with: &str) -> Spliced {
    let mut out = String::with_capacity(text.len() + with.len());
    out.push_str(&text[..range.start]);
    out.push_str(with);
    out.push_str(&text[range.end..]);
    Spliced { text: out, comments_dropped: Vec::new() }
}

/// Insert `rendered` (an event line without its leading indent… it carries one) right before the
/// root's closing `}`. The file's text before the close usually already ends with a line ending;
/// a `{}` root (`createFile`) does not, so one is added — the result always stays one member per
/// line.
fn insert_before_root_close(
    text: &str,
    scan: &Scan,
    rendered: &str,
    eol: &str,
) -> Result<Spliced, SpliceError> {
    let at = scan.root_close;
    let prefix = if text[..at].ends_with(eol) { "" } else { eol };
    let mut out = String::with_capacity(text.len() + rendered.len() + eol.len());
    out.push_str(&text[..at]);
    out.push_str(prefix);
    out.push_str(rendered);
    out.push_str(&text[at..]);
    Ok(Spliced { text: out, comments_dropped: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

    fn crlf(text: &str) -> String {
        text.replace('\n', "\r\n")
    }

    fn sample() -> String {
        crlf(&format!(
            "{HEADER}\n{{\n\t// keep\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t\t// lost\n\t}}\n\t\"b.two\" = \n\t{{\n\t\tbase = \"a.one\"\n\t}}\n}}"
        ))
    }

    #[test]
    fn scan_finds_events_headers_and_comments() {
        let text = sample();
        let scan = scan(&text).expect("scan");
        assert_eq!(scan.eol, "\r\n");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one", "b.two"]);
        assert_eq!(scan.events[0].comment_lines, 1, "the `// lost` line");
        assert_eq!(scan.events[1].comment_lines, 0);
        assert_eq!(&text[scan.root_close..], "}");
        assert_eq!(
            &text[scan.events[0].key_start..scan.events[0].key_end],
            "\"a.one\""
        );
        assert!(scan.events[0].key_quoted);
    }

    #[test]
    fn set_replaces_only_the_span_and_keeps_the_rest_byte_for_byte() {
        let text = sample();
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        object.insert("volume".into(), cs2_kv3::Value::from(0.5f64));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert_eq!(spliced.comments_dropped, [("a.one".to_string(), 1)]);
        // everything outside the touched span is untouched
        let before = text.split("\t\"a.one\"").next().unwrap().to_string();
        let after = spliced.text.split("\t\"a.one\"").next().unwrap().to_string();
        assert_eq!(before, after, "header and the `// keep` comment line");
        assert!(spliced.text.ends_with("\t\"b.two\" = \r\n\t{\r\n\t\tbase = \"a.one\"\r\n\t}\r\n}"));
        // and it still parses, to exactly the intended root
        let reparsed = cs2_kv3::parse(&spliced.text).expect("re-parse");
        let mut intended = cs2_kv3::parse(&text).expect("original parse").root;
        intended.insert("a.one".into(), {
            let mut object = Object::new();
            object.insert("type".into(), Value::from("csgo_mega"));
            object.insert("volume".into(), cs2_kv3::Value::from(0.5f64));
            Value::Object(object)
        });
        assert!(roots_equal(&reparsed.root, &intended));
    }

    #[test]
    fn set_on_a_new_name_inserts_before_the_root_close() {
        let text = sample();
        let mut object = Object::new();
        object.insert("base".into(), Value::from("a.one"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "c.three".into(), object }]).expect("splice");
        assert!(spliced.comments_dropped.is_empty());
        assert!(spliced.text.contains("\t\"c.three\" = \r\n\t{\r\n\t\tbase = \"a.one\"\r\n\t}\r\n}"));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn remove_cuts_the_whole_event_line_and_keeps_the_rest() {
        let text = sample();
        let spliced = splice(&text, &[SpliceOp::Remove { name: "a.one".into() }]).expect("splice");
        assert!(!spliced.text.contains("\"a.one\" = "), "the removed event's span is gone");
        assert!(!spliced.text.contains("// lost"));
        // no stray indent or blank line where the event was (its base reference in b.two stays —
        // a remove does not rewrite other events)
        assert!(spliced.text.contains("\t// keep\r\n\t\"b.two\" = \r\n"));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn rename_rewrites_the_key_and_reference_tokens() {
        let text = sample();
        let spliced =
            splice(&text, &[SpliceOp::Rename { from: "a.one".into(), to: "a.renamed".into() }])
                .expect("splice");
        assert!(spliced.text.contains("\t\"a.renamed\" = \r\n"));
        assert!(spliced.text.contains("base = \"a.renamed\""), "the reference followed");
        assert!(!spliced.text.contains("\"a.one\""));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn a_raw_string_and_a_blob_never_confuse_the_scanner() {
        // The tricky lexemes live in an event that is NOT touched; the removed event sits
        // between them and the root close.
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t\"b.two\" = \n\t{{\n\t\tbase = \"a.one\"\n\t\tnotes = \"not an {{ event }} nor \\\"a key\\\"\"\n\t\traw = \"\"\"\n// not a comment\n\"\"\"\n\t\tblob = #[ 01 02 03 ]\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one", "b.two"]);
        let spliced = splice(&text, &[SpliceOp::Remove { name: "a.one".into() }]).expect("splice");
        assert!(spliced.text.contains("// not a comment"), "the raw string survived");
        assert!(spliced.text.contains("not an { event } nor"), "the string's braces survived");
        assert!(spliced.text.contains("#[ 01 02 03 ]"), "the blob survived");
        assert!(spliced.text.contains("\t\"b.two\" = \r\n"), "b.two's line is intact");
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn a_bare_key_event_keeps_its_style_and_position() {
        let text = crlf(&format!(
            "{HEADER}\n{{\n\tbare_one = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t\"quoted.one\" = \n\t{{\n\t\tbase = \"bare_one\"\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        assert_eq!(scan.events[0].name, "bare_one");
        assert!(!scan.events[0].key_quoted);
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        object.insert("volume".into(), cs2_kv3::Value::from(2i64));
        let spliced = splice(&text, &[SpliceOp::Set { name: "bare_one".into(), object }]).expect("splice");
        assert!(spliced.text.contains("\tbare_one = \r\n\t{"));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn create_file_shape_root_grows_one_member_per_line() {
        let text = format!("{HEADER}\r\n{{}}");
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "new.event".into(), object }]).expect("splice");
        assert!(spliced.text.contains("{\r\n\t\"new.event\" = \r\n\t{\r\n\t\ttype = \"csgo_mega\"\r\n\t}\r\n}"));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // H1: the scanner never slices at a byte that is not a char boundary
    // ---------------------------------------------------------------------------------------

    /// A file whose comments and strings carry multi-byte characters, both inside the event a
    /// test touches and outside it. Before the byte-wise comment scan every one of these made
    /// `scan` panic ("byte index N is not a char boundary") — the first `/* … */` it walked.
    fn multibyte_sample() -> String {
        crlf(&format!(
            "{HEADER}\n{{\n\t/* Música del fondo */\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t\t// Comentário com café\n\t\tdescription = \"Ação & música\"\n\t}}\n\t// Entre os eventos: música\n\t\"b.two\" = \n\t{{\n\t\tbase = \"a.one\"\n\t\tdescription = \"Música: /* não é um comentário */ // tampouco\"\n\t}}\n}}"
        ))
    }

    #[test]
    fn multibyte_comments_and_strings_scan_and_splice() {
        let text = multibyte_sample();
        let scan = scan(&text).expect("scan");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one", "b.two"]);
        assert_eq!(
            scan.events[0].comment_lines, 1,
            "the multi-byte line comment inside a.one"
        );
        assert_eq!(
            scan.events[1].comment_lines, 0,
            "the `/*` inside b.two's string is not a comment"
        );

        // a set on the event that carries the multi-byte comments
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert_eq!(spliced.comments_dropped, [("a.one".to_string(), 1)]);
        // the multi-byte comment lines OUTSIDE the replaced span survive byte-for-byte ...
        assert!(spliced.text.contains("\t/* Música del fondo */\r\n"), "root block comment");
        assert!(spliced.text.contains("// Entre os eventos: música"), "comment between events");
        // ... and the multi-byte string outside the span is still a string, not a comment
        assert!(
            spliced.text.contains("Música: /* não é um comentário */ // tampouco"),
            "the untouched event's string survived verbatim"
        );
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn a_multibyte_comment_inside_a_replaced_span_is_replaced_not_miscounted() {
        // The multi-byte block comment sits inside the event that gets replaced: the splice must
        // replace the whole span (comment included) and count its line, never panic on it.
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\t/* não é um comentário */\n\t\ttype = \"csgo_mega\"\n\t}}\n}}"
        ));
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert!(!spliced.text.contains("não é um comentário"), "the span's comment is gone");
        assert_eq!(spliced.comments_dropped, [("a.one".to_string(), 1)]);
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    #[test]
    fn an_escape_before_a_multibyte_character_never_slices_mid_char() {
        // `\ç` is an unknown escape the lexer keeps verbatim, so `skip_string` steps over two
        // bytes and lands *inside* `ç`: nothing may slice the `str` at that offset (H1).
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t\tdescription = \"A\\ção\"\n\t}}\n\t\"b.two\" = \n\t{{\n\t\tbase = \"a.one\"\n\t}}\n}}"
        ));
        assert!(cs2_kv3::parse(&text).is_ok(), "the escape is legal kv3");
        let scan = scan(&text).expect("scan");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one", "b.two"]);
    }

    // ---------------------------------------------------------------------------------------
    // L3: a flagged scalar value is skipped whatever the spacing
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_flagged_scalar_value_is_skipped_with_and_without_spaces() {
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t\"tight\" = resource:\"mcp.tight\"\n\t\"spaced\" = resource: \"mcp.spaced\"\n\t\"b.two\" = \n\t{{\n\t\tbase = \"a.one\"\n\t\t\"flagged\" = resource:\"mcp.inner\"\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one", "b.two"], "the flagged root members are skipped whole");

        // the flagged members (root level and inside an event) survive a set on a.one
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert!(spliced.text.contains("\t\"tight\" = resource:\"mcp.tight\"\r\n"));
        assert!(spliced.text.contains("\t\"spaced\" = resource: \"mcp.spaced\"\r\n"));
        assert!(spliced.text.contains("\t\t\"flagged\" = resource:\"mcp.inner\"\r\n"));
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // L4: every consumed comment line is counted
    // ---------------------------------------------------------------------------------------

    #[test]
    fn comments_dropped_counts_every_consumed_comment_line() {
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t\t// primeira\n\t\t// segunda\n\t\t/* terceira\n\t\t   quarta */\n\t\tvolume = 0.5\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        assert_eq!(
            scan.events[0].comment_lines, 4,
            "two consecutive line comments (2) plus a two-line block comment (2)"
        );
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert_eq!(spliced.comments_dropped, [("a.one".to_string(), 4)]);
    }

    #[test]
    fn comments_inside_a_nested_value_are_counted_too() {
        // The whole event body is replaced by a `set`, nested arrays included, so a comment
        // inside one of their values is lost with the rest of the span.
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t\t// acima da curva\n\t\tdistance_volume_mapping_curve = \n\t\t[\n\t\t\t// dentro da curva\n\t\t\t[ 0.0, 1.0, ],\n\t\t]\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        assert_eq!(scan.events[0].comment_lines, 2, "the depth-1 comment and the one in the array");
        let mut object = Object::new();
        object.insert("type".into(), Value::from("csgo_mega"));
        let spliced = splice(&text, &[SpliceOp::Set { name: "a.one".into(), object }]).expect("splice");
        assert_eq!(spliced.comments_dropped, [("a.one".to_string(), 2)]);
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // L5: `read_key` skips escapes in a `"…"` key (and not in a `"""…"""` one)
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_quoted_key_with_an_escaped_quote_is_read_whole() {
        let text = "\"a\\\"b\" = 1";
        let mut cursor = Cursor { text, b: text.as_bytes(), i: 0 };
        let (name, quoted) = cursor.read_key().expect("key");
        assert_eq!(name, "a\\\"b", "the escaped quote does not close the key");
        assert!(quoted);
        assert_eq!(&text[cursor.i..], " = 1");
    }

    #[test]
    fn an_escaped_quote_in_a_member_key_does_not_derail_the_scan() {
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"weird\\\"key\" = 5\n\t\"a.one\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n}}"
        ));
        let scan = scan(&text).expect("scan");
        let names: Vec<&str> = scan.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.one"]);
    }

    // ---------------------------------------------------------------------------------------
    // M2: a rename rewrites the key and the `base` / `soundevent_01` references only
    // ---------------------------------------------------------------------------------------

    /// The root `edit_soundevents` intends after a rename: the key moves and only `base` /
    /// `soundevent_01` follow it (the tool's `rewrite_references` — the splice must agree with
    /// it exactly, or the caller's round-trip check refuses the edit).
    fn renamed_root(root: &Object, from: &str, to: &str) -> Object {
        let mut out = Object::new();
        for (key, value) in root.iter() {
            let mut value = value.clone();
            if let Some(object) = value.as_object_mut() {
                if object.get("base").and_then(Value::as_str) == Some(from) {
                    object.insert("base".into(), Value::from(to));
                }
                match object.get_mut("soundevent_01") {
                    Some(Value::String(text)) if text == from => {
                        object.insert("soundevent_01".into(), Value::from(to));
                    }
                    Some(Value::Array(items)) => {
                        for item in items.iter_mut() {
                            if item.as_str() == Some(from) {
                                *item = Value::from(to);
                            }
                        }
                    }
                    _ => {}
                }
            }
            out.insert(if key == from { to.to_string() } else { key.to_string() }, value);
        }
        out
    }

    #[test]
    fn rename_leaves_other_tokens_and_comments_that_hold_the_old_name_alone() {
        // `mcp.a` appears in a `sounds/…` path, in an unrelated `description`, as an exact
        // `vsnd_files_track_01` token and in a comment. Only the key and the `base` /
        // `soundevent_01` references are the document's rename; rewriting anything else would
        // make the spliced file disagree with the intended root (review M2).
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"mcp.a\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t// mcp.a is the base of everything\n\t\"mcp.child\" = \n\t{{\n\t\tbase = \"mcp.a\"\n\t\tsoundevent_01 = \n\t\t[\n\t\t\t\"mcp.a\",\n\t\t\t\"mcp.other\",\n\t\t]\n\t\tvsnd_files_track_01 = \"sounds/mcp.a.vsnd\"\n\t\tdescription = \"mcp.a\"\n\t}}\n\t\"mcp.other.event\" = \n\t{{\n\t\tbase = \"mcp.a\"\n\t\tvsnd_files_track_01 = \"mcp.a\"\n\t}}\n}}"
        ));
        let before = cs2_kv3::parse(&text).expect("original parse");
        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "mcp.a".into(), to: "mcp.renamed".into() }],
        )
        .expect("splice");

        let reparsed = cs2_kv3::parse(&spliced.text).expect("re-parse");
        // the rename succeeded: the spliced file IS the intended root (what the tool checks)
        assert!(
            roots_equal(&reparsed.root, &renamed_root(&before.root, "mcp.a", "mcp.renamed")),
            "the splice matches the document-side rename: {}",
            spliced.text
        );
        // the key moved, in place
        assert!(spliced.text.contains("\t\"mcp.renamed\" = \r\n\t{"));
        // the references followed, in `base` and in the `soundevent_01` array
        let child = reparsed.root.get("mcp.child").and_then(Value::as_object).expect("mcp.child");
        assert_eq!(child.get("base").and_then(Value::as_str), Some("mcp.renamed"));
        assert_eq!(
            child.get("soundevent_01").and_then(Value::as_array).map(|items| {
                items.iter().map(|item| item.as_str().unwrap_or("<not a string>")).collect::<Vec<_>>()
            }),
            Some(vec!["mcp.renamed", "mcp.other"])
        );
        // every other token that happens to hold the old text stayed byte-for-byte
        assert!(spliced.text.contains("vsnd_files_track_01 = \"sounds/mcp.a.vsnd\""));
        assert!(spliced.text.contains("description = \"mcp.a\""));
        assert!(spliced.text.contains("\t\tvsnd_files_track_01 = \"mcp.a\"\r\n"));
        assert!(spliced.text.contains("// mcp.a is the base of everything"));
        assert!(!spliced.text.contains("sounds/mcp.renamed.vsnd"));
        assert!(!spliced.text.contains("description = \"mcp.renamed\""));
    }

    #[test]
    fn rename_rewrites_a_lone_child_reference_and_the_key() {
        // The other half of M2: a reference written as one plain string (`soundevent_01 = "x"`
        // and a `base`) is still rewritten.
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"mcp.a\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t\"mcp.parent\" = \n\t{{\n\t\tsoundevent_01 = \"mcp.a\"\n\t}}\n}}"
        ));
        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "mcp.a".into(), to: "mcp.renamed".into() }],
        )
        .expect("splice");
        assert!(spliced.text.contains("\t\"mcp.renamed\" = \r\n\t{"));
        assert!(spliced.text.contains("soundevent_01 = \"mcp.renamed\""));
        assert!(!spliced.text.contains("\"mcp.a\""));
    }

    #[test]
    fn a_rename_to_a_name_that_is_not_an_identifier_quotes_the_key() {
        // review N6: a bare-keyed event renamed to a digit-leading name — the emitted key must
        // be quoted or the file would not lex.
        let text = crlf(&format!(
            "{HEADER}\n{{\n\tbare_one = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n}}"
        ));
        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "bare_one".into(), to: "1up".into() }],
        )
        .expect("splice");
        assert!(spliced.text.contains("\t\"1up\" = \r\n"), "{:?}", spliced.text);
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
        // and an identifier name keeps the bare style the source had
        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "bare_one".into(), to: "still_bare.2".into() }],
        )
        .expect("splice");
        assert!(spliced.text.contains("\tstill_bare.2 = \r\n"), "{:?}", spliced.text);
        assert!(cs2_kv3::parse(&spliced.text).is_ok());
    }

    // ---------------------------------------------------------------------------------------
    // the committed real-world fixture (Valve's own addon template)
    // ---------------------------------------------------------------------------------------

    /// The committed template (CRLF, 14 events, `base` chains, a `soundevent_01` array, a
    /// `soundevent_01 = ""` and `distance_volume_mapping_curve` arrays of arrays) is the
    /// scanner's real-data check: it must scan, and a rename of the event five others name as
    /// their `base` must come out equal to the document side's intended root.
    #[test]
    fn the_committed_template_scans_and_renames_like_the_document_side() {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/soundevents/soundevents_addon.vsndevts"
        ))
        .expect("the committed addon template fixture");
        let before = cs2_kv3::parse(&text).expect("the template parses");
        let scan = scan(&text).expect("the template scans");
        assert!(scan.events.len() >= 14, "the template's events: {}", scan.events.len());
        assert_eq!(scan.eol, "\r\n");

        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "amb.base".into(), to: "amb.base.renamed".into() }],
        )
        .expect("splice");
        let reparsed = cs2_kv3::parse(&spliced.text).expect("the spliced template parses");
        assert!(
            roots_equal(&reparsed.root, &renamed_root(&before.root, "amb.base", "amb.base.renamed")),
            "the splice matches the document-side rename"
        );
        assert_eq!(
            spliced.text.matches("base = \"amb.base.renamed\"").count(),
            5,
            "the five `base = \"amb.base\"` references followed"
        );
        assert!(!spliced.text.contains("\"amb.base\""));
    }

    #[test]
    fn a_raw_string_reference_is_rewritten_like_the_document_side() {
        // `base = """…"""` decodes to the name, so the document side rewrites it; the splice must
        // replace the raw token with one that decodes to the new name (or the round-trip check
        // would refuse the edit).
        let text = crlf(&format!(
            "{HEADER}\n{{\n\t\"mcp.a\" = \n\t{{\n\t\ttype = \"csgo_mega\"\n\t}}\n\t\"mcp.parent\" = \n\t{{\n\t\tbase = \"\"\"\nmcp.a\n\"\"\"\n\t\tvolume = 0.5\n\t}}\n}}"
        ));
        let before = cs2_kv3::parse(&text).expect("original parse");
        assert_eq!(
            before.root.get("mcp.parent").and_then(Value::as_object).and_then(|o| o.get("base")).and_then(Value::as_str),
            Some("mcp.a"),
            "the raw string really holds the name"
        );
        let spliced = splice(
            &text,
            &[SpliceOp::Rename { from: "mcp.a".into(), to: "mcp.renamed".into() }],
        )
        .expect("splice");
        let reparsed = cs2_kv3::parse(&spliced.text).expect("re-parse");
        assert!(
            roots_equal(&reparsed.root, &renamed_root(&before.root, "mcp.a", "mcp.renamed")),
            "the raw reference followed the rename: {}",
            spliced.text
        );
    }
}
