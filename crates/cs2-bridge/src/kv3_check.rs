//! `bridge/kv3_check.h` in Rust: the bridge's pre-load gate for KV3 text, plus `pet.open`'s
//! host-path derivation.
//!
//! # Why the gate exists
//!
//! `pet`'s `CPetSession::ReloadDocument` (session vtable slot 7,
//! `S2_SLOT_CBaseEditorSession_ReloadDocument`) on a file pet cannot parse does not stop at "a
//! document with no root": live 2026-10-13 it recursed until the Wine main thread's stack
//! overflowed and the process wedged. Three inputs did that (the whole file `{{{ not kv3\n`; a
//! valid kv3 header followed by a truncated body; a *balanced* document with a key and no value),
//! and a wrong root `_class` failed fast (`c0000409`). The C bridge therefore refuses the file
//! itself, with a short static reason, before pet ever opens it. This module is that gate.
//!
//! # The contract
//!
//! [`kv3_check`] and [`vpcf_check`] take the file's bytes (`&[u8]`, not `&str`: a NUL byte is a
//! refusal, not a terminator, and the C gate never validated UTF-8) and answer `Ok(())` when pet
//! can load the file or `Err(`[`Refusal`]`)` naming the reason. The reason strings are the
//! protocol: the bridge reports them in `-32602 UnloadableFile {reason}`, so [`Reason::as_str`]
//! returns exactly the C literals.
//!
//! * [`kv3_check`]: a `<!-- … kv3 … -->` header naming any `format:`, then the KV3-text grammar
//!   (exactly one root value, which must be an object).
//! * [`vpcf_check`]: the same, plus `format:vpcf` in the header and the root object's own
//!   `_class` member equal to `CParticleSystemDefinition` (exact case: the class name is a schema
//!   lookup key, while *member names* are hashed case-insensitively upstream, so `_CLASS` is the
//!   same member).
//!
//! The grammar is deliberately strict on structure and lenient on token content: members are
//! `key = value` separated by whitespace (no commas between members), arrays are comma-separated
//! (a trailing comma is legal, a leading one is not), and strings/raw strings/numbers/literals/
//! flagged values/blob tokens are the value kinds. Comments are allowed wherever whitespace is.
//!
//! # Why this is not `cs2_kv3::parse`
//!
//! The workspace's `cs2-kv3` reader is a superset acceptor: it was widened (see its crate docs,
//! "The union") to read everything the pre-conformance reader and ValveKeyValue's reader accept,
//! which includes spellings this gate must refuse (`{ a = 1, b = 2 }`, `[ 1 2 ]`, a chained flag,
//! a space after a flag's colon, a literal used as a flag name, single quotes, `inf`, a leading
//! `.` key …), and it requires a stricter header token stream than the gate (`format:` must be
//! present, so `<!-- kv3 -->` is refused there and accepted here). Making `cs2-kv3` strict would
//! change what its two consumers (hammer-mcp's ModelDoc and soundevents code) accept, so the gate
//! is a faithful port of the C state machine instead; `crates/cs2-bridge/tests/kv3_check.rs`
//! carries the full disagreement table as an executable check against `cs2_kv3::parse`.
//!
//! # Bounds
//!
//! [`DEPTH_CAP`] is `KV3_CHECK_DEPTH_CAP` (256). It is enforced on the *raw* token stream before
//! the grammar runs, so the recursive descent below can never recurse on `{{{{…`; the deepest
//! real content file nests 19 levels.
//!
//! # pet.open's host path
//!
//! [`addon_name`], [`app_root`] and [`host_path`] are the pure `<app>\content\csgo_addons\<addon>\
//! <path>` derivation the gate resolves with, ported from the same header. They are pure so the
//! host tests cover what would otherwise only run live.

use core::fmt;

/// The deepest nesting [`kv3_check`] accepts: the number of open `{` / `[` in the raw token
/// stream. `KV3_CHECK_DEPTH_CAP` in `bridge/kv3_check.h`; 256 is far deeper than any real
/// document (19 is the deepest in the installed content tree).
pub const DEPTH_CAP: usize = 256;

/// Why the gate refused a file.
///
/// Every variant is one of the C header's static reasons, spelled the same way by
/// [`Reason::as_str`] because the bridge puts that string into the protocol's `-32602
/// UnloadableFile {reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The file does not start with a `<!-- … kv3 … -->` header (or the header never closes).
    NoKv3Header,
    /// [`vpcf_check`] only: the header does not carry `format:vpcf`.
    NotVpcf,
    /// A NUL byte anywhere in the file: this is binary, not KV3 text.
    Binary,
    /// More than [`DEPTH_CAP`] open brackets in the raw token stream.
    TooDeep,
    /// A `/*` block comment that never closes.
    UnterminatedComment,
    /// A `"…"` or `"""…"""` string that never closes.
    UnterminatedString,
    /// The body's first token is not `{`.
    NoRootObject,
    /// Content after the root object: a second root, or a stray `}`.
    TrailingContent,
    /// A `{`/`[` closed by the wrong bracket, or the file ended inside one.
    Unbalanced,
    /// A `,` between two object members.
    CommaInObject,
    /// A key position that starts with neither a letter/`_` nor `"`.
    BadKey,
    /// A key without its `=` (a `name:value` flag is not a member).
    ExpectedEquals,
    /// A value position with no value (a key with none, a flag with none, a stray `:`).
    ExpectedValue,
    /// An array element not followed by `,` or `]`.
    ExpectedComma,
    /// A token that is not a value: a bare word, a broken number, a malformed blob, a chained
    /// flag.
    BadToken,
    /// [`vpcf_check`] only: the root object has no `_class` member.
    NoRootClass,
    /// [`vpcf_check`] only: the root `_class` is not exactly `CParticleSystemDefinition` (or not
    /// a bare string).
    WrongRootClass,
}

impl Reason {
    /// The reason exactly as the C bridge reports it (`bridge/kv3_check.h`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Reason::NoKv3Header => "no_kv3_header",
            Reason::NotVpcf => "not_vpcf",
            Reason::Binary => "binary",
            Reason::TooDeep => "too_deep",
            Reason::UnterminatedComment => "unterminated_comment",
            Reason::UnterminatedString => "unterminated_string",
            Reason::NoRootObject => "no_root_object",
            Reason::TrailingContent => "trailing_content",
            Reason::Unbalanced => "unbalanced",
            Reason::CommaInObject => "comma_in_object",
            Reason::BadKey => "bad_key",
            Reason::ExpectedEquals => "expected_equals",
            Reason::ExpectedValue => "expected_value",
            Reason::ExpectedComma => "expected_comma",
            Reason::BadToken => "bad_token",
            Reason::NoRootClass => "no_root_class",
            Reason::WrongRootClass => "wrong_root_class",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal: the C gate's reason, plus where this port noticed it.
///
/// The C gate reports the reason alone (no position), so [`Refusal::at`] is diagnostic only - a
/// byte offset into the buffer, for the bridge's log line. It is never part of the protocol
/// contract and the tests compare reasons, not offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    /// The C gate's reason.
    pub reason: Reason,
    /// Byte offset in the buffer where the refusal was raised (diagnostic only).
    pub at: usize,
}

impl Refusal {
    const fn new(reason: Reason, at: usize) -> Self {
        Self { reason, at }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.reason.as_str(), self.at)
    }
}

impl std::error::Error for Refusal {}

/// `kv3_check`: a KV3-text document with a kv3 header, any `format:`.
///
/// `Ok(())` when pet's loader is written for the file's shape, `Err(refusal)` otherwise.
pub fn kv3_check(buf: &[u8]) -> Result<(), Refusal> {
    check(buf, false)
}

/// `vpcf_check`: the above with `format:vpcf` in the header and the root object's own `_class`
/// equal to `CParticleSystemDefinition`.
pub fn vpcf_check(buf: &[u8]) -> Result<(), Refusal> {
    check(buf, true)
}

/// The whitespace the C gate skips: the ASCII whitespace set *plus* the vertical tab, which
/// `char::is_ascii_whitespace` does not include.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

/// One byte of a bare identifier (`[A-Za-z0-9_.]` after the first character).
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

/// 1 when `word` occurs in `b[from..to)`; the C `kv3_has`.
fn has(b: &[u8], from: usize, to: usize, word: &[u8]) -> bool {
    if word.is_empty() || to < from || to - from < word.len() {
        return false;
    }
    b[from..to].windows(word.len()).any(|window| window == word)
}

/// ASCII-case-insensitive equality, exact length; the C `kv3_ci_eq`.
fn ci_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

/// The length of the number at `b[i..]`: int or float, optional sign, optional exponent (which
/// needs its own digits). 0 when it is not a number; the C `kv3_number`.
fn number(b: &[u8], i: usize) -> usize {
    let n = b.len();
    let mut j = i;
    let mut digits = 0usize;
    if j < n && (b[j] == b'+' || b[j] == b'-') {
        j += 1;
    }
    while j < n && b[j].is_ascii_digit() {
        j += 1;
        digits += 1;
    }
    if j < n && b[j] == b'.' {
        j += 1;
        while j < n && b[j].is_ascii_digit() {
            j += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return 0;
    }
    if j < n && (b[j] == b'e' || b[j] == b'E') {
        let mut k = j + 1;
        let mut exponent_digits = 0usize;
        if k < n && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        while k < n && b[k].is_ascii_digit() {
            k += 1;
            exponent_digits += 1;
        }
        if exponent_digits > 0 {
            j = k; // `1e` is not a number: stop before the `e`.
        }
    }
    j - i
}

/// The raw-token depth pass: the cap is enforced before the grammar can recurse, and strings and
/// comments are skipped whole so brackets inside them do not count. Balance itself is the
/// grammar's job. The C `kv3_shape`.
fn shape(b: &[u8], mut i: usize) -> Result<(), Refusal> {
    let n = b.len();
    let mut depth = 0usize;
    while i < n {
        let c = b[i];
        if is_space(c) {
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i += 2;
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let mut j = i + 2;
            while j + 1 < n && !(b[j] == b'*' && b[j + 1] == b'/') {
                j += 1;
            }
            if j + 1 >= n {
                return Err(Refusal::new(Reason::UnterminatedComment, i));
            }
            i = j + 2;
            continue;
        }
        if c == b'"' {
            if i + 2 < n && b[i + 1] == b'"' && b[i + 2] == b'"' {
                let mut j = i + 3;
                loop {
                    if j + 2 >= n {
                        return Err(Refusal::new(Reason::UnterminatedString, i));
                    }
                    if b[j] == b'"' && b[j + 1] == b'"' && b[j + 2] == b'"' {
                        break;
                    }
                    j += 1;
                }
                i = j + 3;
            } else {
                let mut j = i + 1;
                loop {
                    if j >= n {
                        return Err(Refusal::new(Reason::UnterminatedString, i));
                    }
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            continue;
        }
        if c == b'{' || c == b'[' {
            if depth >= DEPTH_CAP {
                return Err(Refusal::new(Reason::TooDeep, i));
            }
            depth += 1;
            i += 1;
            continue;
        }
        if c == b'}' || c == b']' {
            depth = depth.saturating_sub(1); // balance is the grammar pass's job
            i += 1;
            continue;
        }
        i += 1;
    }
    Ok(())
}

/// The whole gate: NUL scan, header, depth pass, then the grammar from `Kv3` in the C header.
fn check(buf: &[u8], want_vpcf: bool) -> Result<(), Refusal> {
    if let Some(at) = buf.iter().position(|byte| *byte == 0) {
        return Err(Refusal::new(Reason::Binary, at));
    }
    let n = buf.len();
    let mut i = 0usize;
    if n >= 3 && buf[0] == 0xef && buf[1] == 0xbb && buf[2] == 0xbf {
        i = 3; // a UTF-8 BOM before the header
    }
    while i < n && is_space(buf[i]) {
        i += 1;
    }
    if i + 4 > n || &buf[i..i + 4] != b"<!--" {
        return Err(Refusal::new(Reason::NoKv3Header, i));
    }
    i += 4;
    let header_start = i;
    while i + 3 <= n && &buf[i..i + 3] != b"-->" {
        i += 1;
    }
    if i + 3 > n {
        return Err(Refusal::new(Reason::NoKv3Header, header_start));
    }
    if !has(buf, header_start, i, b"kv3") {
        return Err(Refusal::new(Reason::NoKv3Header, header_start));
    }
    if want_vpcf && !has(buf, header_start, i, b"format:vpcf") {
        return Err(Refusal::new(Reason::NotVpcf, header_start));
    }
    i += 3;
    shape(buf, i)?;

    let mut scanner = Scanner { b: buf, i, depth: 0, root_seen: false, root_class_ok: false };
    scanner.trivia()?;
    if scanner.i >= n || buf[scanner.i] != b'{' {
        return Err(Refusal::new(Reason::NoRootObject, scanner.i));
    }
    scanner.object(true)?;
    scanner.trivia()?;
    if scanner.i < n {
        return Err(Refusal::new(Reason::TrailingContent, scanner.i));
    }
    if want_vpcf {
        if !scanner.root_seen {
            return Err(Refusal::new(Reason::NoRootClass, scanner.i));
        }
        if !scanner.root_class_ok {
            return Err(Refusal::new(Reason::WrongRootClass, scanner.i));
        }
    }
    Ok(())
}

/// The grammar pass: recursive descent over the token stream, one cursor. The C `Kv3`.
struct Scanner<'a> {
    /// The whole buffer, indexed (never a C string).
    b: &'a [u8],
    /// The cursor.
    i: usize,
    /// Open objects + arrays (the raw depth pass already bounded this by [`DEPTH_CAP`]).
    depth: usize,
    /// The ROOT object has a `_class` member.
    root_seen: bool,
    /// Every root `_class` member is exactly the class name.
    root_class_ok: bool,
}

impl<'a> Scanner<'a> {
    fn refusal(&self, reason: Reason) -> Refusal {
        Refusal::new(reason, self.i)
    }

    /// Skip whitespace and comments; an unterminated block comment is a refusal. The C
    /// `kv3_trivia`.
    fn trivia(&mut self) -> Result<(), Refusal> {
        loop {
            if self.i >= self.b.len() {
                return Ok(());
            }
            let c = self.b[self.i];
            if is_space(c) {
                self.i += 1;
                continue;
            }
            if c == b'/' && self.i + 1 < self.b.len() && self.b[self.i + 1] == b'/' {
                self.i += 2;
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            if c == b'/' && self.i + 1 < self.b.len() && self.b[self.i + 1] == b'*' {
                let mut j = self.i + 2;
                while j + 1 < self.b.len() && !(self.b[j] == b'*' && self.b[j + 1] == b'/') {
                    j += 1;
                }
                if j + 1 >= self.b.len() {
                    return Err(self.refusal(Reason::UnterminatedComment));
                }
                self.i = j + 2;
                continue;
            }
            return Ok(());
        }
    }

    /// One `"…"` string (escapes) or a `"""…"""` raw string at the cursor. On success the span is
    /// the text *inside* a plain string, and `None` for a raw string (text, not a name). The C
    /// `kv3_string`.
    fn string(&mut self) -> Result<Option<(usize, usize)>, Refusal> {
        let n = self.b.len();
        if self.i + 2 < n && self.b[self.i + 1] == b'"' && self.b[self.i + 2] == b'"' {
            let mut j = self.i + 3;
            loop {
                if j + 2 >= n {
                    return Err(self.refusal(Reason::UnterminatedString));
                }
                if self.b[j] == b'"' && self.b[j + 1] == b'"' && self.b[j + 2] == b'"' {
                    break;
                }
                j += 1;
            }
            self.i = j + 3;
            return Ok(None);
        }
        let mut j = self.i + 1;
        loop {
            if j >= n {
                return Err(self.refusal(Reason::UnterminatedString));
            }
            if self.b[j] == b'\\' {
                j += 2;
                continue;
            }
            if self.b[j] == b'"' {
                break;
            }
            j += 1;
        }
        let text = (self.i + 1, j);
        self.i = j + 1;
        Ok(Some(text))
    }

    /// One binary blob `#[ hex bytes ]` (whitespace between the bytes, nothing else). The C
    /// `kv3_blob`.
    fn blob(&mut self) -> Result<(), Refusal> {
        let n = self.b.len();
        if self.i + 1 >= n || self.b[self.i + 1] != b'[' {
            return Err(self.refusal(Reason::BadToken));
        }
        let mut j = self.i + 2;
        loop {
            if j >= n {
                return Err(Refusal::new(Reason::Unbalanced, j)); // the blob never closes
            }
            let c = self.b[j];
            if c == b']' {
                break;
            }
            if !(c.is_ascii_hexdigit() || is_space(c)) {
                return Err(Refusal::new(Reason::BadToken, j));
            }
            j += 1;
        }
        self.i = j + 1;
        Ok(())
    }

    /// One value token, with the cursor past any trivia. `allow_flag`: a `name:value` flag is
    /// legal here. The flag's own value is parsed with `allow_flag = false` and NO trivia, so
    /// flags cannot chain (`a:b:"x"`), a bare `name:` is refused, and `resource: "x"` (space
    /// after the colon) is refused. The span is a plain string's text only (a bare string is the
    /// only thing that can be a class name). The C `kv3_value_token`.
    fn value_token(&mut self, allow_flag: bool) -> Result<Option<(usize, usize)>, Refusal> {
        let n = self.b.len();
        if self.i >= n {
            return Err(self.refusal(if self.depth > 0 {
                Reason::Unbalanced
            } else {
                Reason::ExpectedValue
            }));
        }
        let c = self.b[self.i];
        match c {
            b'{' => {
                self.object(false)?;
                return Ok(None);
            }
            b'[' => {
                self.array()?;
                return Ok(None);
            }
            b'"' => return self.string(),
            b'#' => {
                self.blob()?;
                return Ok(None);
            }
            _ => {}
        }
        if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' {
            let len = number(self.b, self.i);
            if len == 0 {
                return Err(self.refusal(Reason::BadToken));
            }
            self.i += len;
            return Ok(None);
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.i;
            let mut end = self.i;
            while end < n && is_ident_byte(self.b[end]) {
                end += 1;
            }
            let word = &self.b[start..end];
            if word == b"true" || word == b"false" || word == b"null" {
                self.i = end;
                return Ok(None); // a literal wins over a flag of the same name
            }
            if allow_flag && end < n && self.b[end] == b':' {
                self.i = end + 1;
                // The C passes NULL out-params here (`kv3_value_token(p, 0, 0, 0)`), so a flagged
                // value is never a class name, even when it wraps a bare string.
                self.value_token(false)?;
                return Ok(None);
            }
            return Err(Refusal::new(Reason::BadToken, start)); // a bare word is not a value
        }
        Err(self.refusal(Reason::ExpectedValue))
    }

    /// One value, trivia skipped first (every value position except a flag's own). The C
    /// `kv3_value`.
    fn value(&mut self, allow_flag: bool) -> Result<Option<(usize, usize)>, Refusal> {
        self.trivia()?;
        self.value_token(allow_flag)
    }

    /// An object: `{ key = value … }`, no commas between members. `is_root` makes the object's
    /// own `_class` members count towards the vpcf root-class check. The C `kv3_object`.
    fn object(&mut self, is_root: bool) -> Result<(), Refusal> {
        let n = self.b.len();
        self.i += 1; // the `{`
        self.depth += 1;
        loop {
            self.trivia()?;
            if self.i >= n {
                return Err(self.refusal(Reason::Unbalanced)); // truncated inside the object
            }
            let c = self.b[self.i];
            if c == b'}' {
                self.i += 1;
                self.depth -= 1;
                return Ok(());
            }
            if c == b']' {
                return Err(self.refusal(Reason::Unbalanced));
            }
            if c == b',' {
                return Err(self.refusal(Reason::CommaInObject));
            }
            let is_class = if c == b'"' {
                let span = self.string()?;
                is_root
                    && span.is_some_and(|(start, end)| ci_eq(&self.b[start..end], b"_class"))
            } else if c.is_ascii_alphabetic() || c == b'_' {
                let start = self.i;
                while self.i < n && is_ident_byte(self.b[self.i]) {
                    self.i += 1;
                }
                is_root && ci_eq(&self.b[start..self.i], b"_class")
            } else {
                return Err(self.refusal(Reason::BadKey));
            };
            self.trivia()?;
            if self.i >= n {
                return Err(self.refusal(Reason::Unbalanced));
            }
            if self.b[self.i] != b'=' {
                return Err(self.refusal(Reason::ExpectedEquals));
            }
            self.i += 1;
            let value = self.value(true)?; // the value is required
            if is_class {
                // Only the ROOT object's own `_class`, and every one of them counts.
                let ok = value.is_some_and(|(start, end)| {
                    &self.b[start..end] == b"CParticleSystemDefinition"
                });
                if !self.root_seen {
                    self.root_seen = true;
                    self.root_class_ok = ok;
                } else {
                    self.root_class_ok = self.root_class_ok && ok;
                }
            }
        }
    }

    /// An array: `[ value, value, ]` - commas between elements, a trailing comma legal. The C
    /// `kv3_array`.
    fn array(&mut self) -> Result<(), Refusal> {
        let n = self.b.len();
        self.i += 1; // the `[`
        self.depth += 1;
        loop {
            self.trivia()?;
            if self.i >= n {
                return Err(self.refusal(Reason::Unbalanced));
            }
            let c = self.b[self.i];
            if c == b']' {
                self.i += 1;
                self.depth -= 1;
                return Ok(());
            }
            if c == b'}' {
                return Err(self.refusal(Reason::Unbalanced));
            }
            self.value(true)?;
            self.trivia()?;
            if self.i >= n {
                return Err(self.refusal(Reason::Unbalanced));
            }
            match self.b[self.i] {
                b',' => self.i += 1, // a trailing comma is legal (Valve writes one)
                b']' => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(());
                }
                _ => return Err(self.refusal(Reason::ExpectedComma)),
            }
        }
    }
}

// ---- pet.open's host path: <app>\content\csgo_addons\<addon>\<rel with '/' -> '\'> ----

/// The cap the C host tests give the path buffers (`char b[512]` in `test_kv3_check.c`).
const PATH_CAP: usize = 512;

/// The bytes before the first NUL of a buffer the `*_into` helpers filled.
fn c_str(buf: &[u8]) -> &[u8] {
    let len = buf.iter().position(|byte| *byte == 0).unwrap_or(buf.len());
    &buf[..len]
}

/// A buffer the `*_into` helpers filled, as a `String`.
///
/// The input is always a `&str` and the cut is always at a path component boundary (the
/// separators are ASCII), so the bytes are valid UTF-8; a failure here is an internal bug, not
/// bad input, and is loud rather than lossily repaired.
fn c_string(buf: &[u8]) -> String {
    core::str::from_utf8(c_str(buf))
        .expect("the *_into helpers copy a &str prefix at an ASCII separator")
        .to_string()
}

/// Append `s` to `out[*len..]`, keeping `out` NUL-terminated; false when it does not fit. The C
/// `kv3_append`.
fn append(out: &mut [u8], len: &mut usize, s: &[u8]) -> bool {
    if out.is_empty() {
        return false;
    }
    for &byte in s {
        if *len + 1 >= out.len() {
            return false;
        }
        out[*len] = byte;
        *len += 1;
    }
    out[*len] = 0;
    true
}

/// The value of `-addon <name>` in a command line, into `out` (whose length is the C `cap`).
///
/// `-addon mcp_test` (the runtime's form), `-addon=mcp_test` or `-addon "mcp_test"`; the name may
/// only be `[A-Za-z0-9_]` (a content directory name). False when it is absent, empty, or not that
/// shape - the C `no_addon`.
pub fn addon_name_into(cmdline: &str, out: &mut [u8]) -> bool {
    let c = cmdline.as_bytes();
    let cap = out.len();
    if cap == 0 {
        return false;
    }
    let mut k = 0usize;
    while k < c.len() {
        if c[k] == b'-'
            && k + 6 <= c.len()
            && ci_eq(&c[k + 1..k + 6], b"addon")
            && matches!(c.get(k + 6), Some(b' ') | Some(b'\t') | Some(b'=') | Some(b'"'))
        {
            let mut p = k + 6;
            if c[p] == b'=' {
                p += 1;
            }
            while p < c.len() && (c[p] == b' ' || c[p] == b'\t') {
                p += 1;
            }
            let mut n = 0usize;
            if p < c.len() && c[p] == b'"' {
                // The whole quoted token must be the name.
                p += 1;
                while p < c.len() && c[p] != b'"' {
                    let ch = c[p];
                    if !(ch.is_ascii_alphanumeric() || ch == b'_') {
                        return false;
                    }
                    if n + 1 >= cap {
                        return false;
                    }
                    out[n] = ch;
                    n += 1;
                    p += 1;
                }
                if p >= c.len() {
                    return false; // unterminated
                }
                p += 1;
                if p < c.len() && c[p] != b' ' && c[p] != b'\t' {
                    return false;
                }
            } else {
                while p < c.len() && c[p] != b' ' && c[p] != b'\t' && c[p] != b'"' {
                    let ch = c[p];
                    if !(ch.is_ascii_alphanumeric() || ch == b'_') {
                        return false;
                    }
                    if n + 1 >= cap {
                        return false;
                    }
                    out[n] = ch;
                    n += 1;
                    p += 1;
                }
            }
            if n == 0 {
                return false;
            }
            out[n] = 0;
            return true;
        }
        k += 1;
    }
    false
}

/// [`addon_name_into`] with the C host test's 512-byte buffer; `None` where the C returns 0.
pub fn addon_name(cmdline: &str) -> Option<String> {
    let mut buf = [0u8; PATH_CAP];
    addon_name_into(cmdline, &mut buf).then(|| c_string(&buf))
}

/// `<app>` out of a module path, into `out` (whose length is the C `cap`).
///
/// The last four path components must be `game\bin\win64\cs2.exe` (ASCII-case-insensitively, both
/// separators), the rest is `<app>`. False when the path is not that layout - the C
/// `no_app_root`.
pub fn app_root_into(exe: &str, out: &mut [u8]) -> bool {
    let b = exe.as_bytes();
    let n = b.len();
    let cap = out.len();
    if n == 0 || cap == 0 || n >= 1024 {
        return false;
    }
    // The start offset of each component, the last five kept.
    let mut w = [0usize; 5];
    let mut wc = 0usize;
    let mut s = 0usize;
    loop {
        let mut e = s;
        while e < n && b[e] != b'\\' && b[e] != b'/' {
            e += 1;
        }
        if wc < 5 {
            w[wc] = s;
            wc += 1;
        } else {
            w = [w[1], w[2], w[3], w[4], s];
        }
        if e >= n {
            break;
        }
        s = e + 1;
    }
    if wc < 4 {
        return false;
    }
    // The length of the component starting at w[k] when a following component exists is
    // `w[k + 1] - w[k] - 1` (one separator); the last one runs to the end.
    let component = |k: usize| -> &[u8] {
        if k + 1 < wc {
            &b[w[k]..w[k + 1] - 1]
        } else {
            &b[w[k]..n]
        }
    };
    if !ci_eq(component(wc - 4), b"game")
        || !ci_eq(component(wc - 3), b"bin")
        || !ci_eq(component(wc - 2), b"win64")
        || !ci_eq(component(wc - 1), b"cs2.exe")
    {
        return false;
    }
    let mut alen = w[wc - 4]; // everything before the `game` component
    if alen > 0 && (b[alen - 1] == b'\\' || b[alen - 1] == b'/') {
        alen -= 1;
    }
    if alen == 0 || alen + 1 > cap {
        return false;
    }
    out[..alen].copy_from_slice(&b[..alen]);
    out[alen] = 0;
    true
}

/// [`app_root_into`] with the C host test's 512-byte buffer; `None` where the C returns 0.
pub fn app_root(exe: &str) -> Option<String> {
    let mut buf = [0u8; PATH_CAP];
    app_root_into(exe, &mut buf).then(|| c_string(&buf))
}

/// `<app>\content\csgo_addons\<addon>\<rel with '/' -> '\'>`, into `out` (whose length is the C
/// `cap`).
pub fn host_path_into(app: &str, addon: &str, rel: &str, out: &mut [u8]) -> bool {
    if app.is_empty() || addon.is_empty() || rel.is_empty() || out.is_empty() {
        return false;
    }
    out[0] = 0;
    let mut n = 0usize;
    if !append(out, &mut n, app.as_bytes())
        || !append(out, &mut n, b"\\content\\csgo_addons\\")
        || !append(out, &mut n, addon.as_bytes())
        || !append(out, &mut n, b"\\")
    {
        return false;
    }
    for &byte in rel.as_bytes() {
        if n + 1 >= out.len() {
            return false;
        }
        out[n] = if byte == b'/' { b'\\' } else { byte };
        n += 1;
        out[n] = 0;
    }
    true
}

/// [`host_path_into`] with the C host test's 512-byte buffer; `None` where the C returns 0.
pub fn host_path(app: &str, addon: &str, rel: &str) -> Option<String> {
    let mut buf = [0u8; PATH_CAP];
    host_path_into(app, addon, rel, &mut buf).then(|| c_string(&buf))
}
