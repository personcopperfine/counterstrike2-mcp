//! Token scanner: KV3 text -> tokens, tracking 1-based positions.
//!
//! A port of ValveKeyValue's `KV3TokenReader` (MIT, commit 28ca0e5), widened into the union the
//! repository needs (see `# The union` in the crate docs): comments (`//`, `/* … */`) and
//! whitespace collapse into nothing; an unquoted token is a run in the 686975d shape (an
//! identifier run `[A-Za-z0-9_.]` after a letter or `_`, otherwise a number run
//! `[A-Za-z0-9_.+-]`), so it ends at `//`, `/*`, `#`, `-` after a name and the like, exactly
//! where 686975d ended it; an identifier-shaped token followed by (whitespace and comments and)
//! `:` or `|` becomes a flag token whose separator is already consumed.
//! Strings: `"` and `'` behave alike (escapes are `\n`, `\t`, `\r`, `\"`, `\\` — the cs2-kv3
//! escape table; any other `\X` keeps the backslash and the character, and real newlines are
//! kept), `"""` opens a raw multi-line string, `#[ … ]` a byte blob. Numbers are *not* lexed
//! here: like upstream, an unquoted token is handed to the parser, which decides
//! literal/number/error by shape (`crate::is_number_lexeme` mirrors upstream's `ParseValue`).
//!
//! # Differences from upstream
//!
//! * Blob bytes accumulate as hex **nibbles** across trivia instead of per-byte two-hex-digit
//!   tokens: the pre-conformance reader (commit 686975d) read `#[ 0102 ]` as two bytes
//!   `0x01 0x02` and `#[ 4 1 ]` as `0x41`, and the reader must accept the union of what 686975d
//!   and upstream accepted (WP5 round 2). Upstream rejects those two spellings; malformed
//!   input both refuse (`#[ AB C ]` odd count, `#[ 4Z ]` not hex) stays refused, and comments
//!   between the nibbles stay allowed (upstream's leniency).
//! * A `//` comment ending in `\` does **not** continue onto the next line (upstream's
//!   `SkipComment` continues it): 686975d ended it at the newline, and Windows paths in
//!   comments (`// see C:\models\`) must not swallow the following entry.
//! * `"""` strings close at the **first** `"""` and a lone `\r` becomes `\n` (686975d), so a
//!   file 686975d accepted reads as it did. Only when the whole file then fails to parse is it
//!   lexed again with upstream's rule (opening newline required, closing `"""` only on a line
//!   of its own, `\"""` un-escaped), which keeps those files readable too. A file both rules
//!   accept but read differently reads the 686975d way.
//! * Unknown string escapes keep their backslash (`\x` stays `\x`); upstream drops it. Same
//!   reason as the blobs: 686975d's escape table wins where the two disagree (WP5 round 2 item
//!   3), while the *acceptance* over the rest of the grammar is upstream's plus 686975d's.

use crate::ParseError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// An unquoted token: a bare key, a `null`/`true`/`false` literal, a flag name or a number
    /// lexeme — the parser classifies it by position and shape (upstream: the token type
    /// follows what the source looks like, not the contents).
    Ident(String),
    /// A `"…"` or `'…'` string, decoded.
    Quoted(String),
    /// A `"""…"""` string, decoded (line endings normalised to `\n`).
    Raw(String),
    /// A `#[ … ]` blob, decoded.
    Blob(Vec<u8>),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Eq,
    Comma,
    /// A flag whose name and separator (`:` or `|`) were consumed together (`resource:"x"`).
    Flag(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub(crate) kind: Kind,
    pub(crate) line: usize,
    pub(crate) col: usize,
}

impl Token {
    pub(crate) fn what(&self) -> String {
        match &self.kind {
            Kind::Ident(s) => format!("`{s}`"),
            Kind::Quoted(s) => format!("string {s:?}"),
            Kind::Raw(_) => "a multi-line string".to_string(),
            Kind::Blob(b) => format!("a {} byte blob", b.len()),
            Kind::LBrace => "`{`".to_string(),
            Kind::RBrace => "`}`".to_string(),
            Kind::LBracket => "`[`".to_string(),
            Kind::RBracket => "`]`".to_string(),
            Kind::Eq => "`=`".to_string(),
            Kind::Comma => "`,`".to_string(),
            Kind::Flag(flag) => format!("flag `{flag}`"),
        }
    }
}

/// Upstream `IsUnquotedTokenChar`: printable ASCII except the structural characters. A token
/// also stops at `'`/`"`/`:`/`|`/`;` — those open a string, close a flag or are errors.
pub(crate) fn is_unquoted_token_char(c: char) -> bool {
    c > ' '
        && (c as u32) < 0x80
        && !matches!(
            c,
            '{' | '}' | '[' | ']' | '=' | ',' | '\'' | '"' | ':' | '|' | ';'
        )
}

/// Whitespace and comments collapse into nothing (upstream `SwallowWhitespaceAndComments`):
/// a `//` comment runs to the end of the line (no `\` continuation, see below) and a
/// `/* … */` comment may span lines; a stray `/` is an error.
/// On return the cursor sits on the next non-trivia character; `line`/`col` track it 1-based.
pub(crate) fn skip_trivia(
    src: &str,
    at: &mut usize,
    line: &mut usize,
    col: &mut usize,
) -> Result<(), ParseError> {
    loop {
        let c = match src[*at..].chars().next() {
            Some(c) => c,
            None => return Ok(()),
        };
        if c.is_whitespace() {
            bump(src, at, line, col);
            continue;
        }
        let second = src[*at..].chars().nth(1);
        if c == '/' && second == Some('/') {
            bump(src, at, line, col);
            bump(src, at, line, col);
            // A `//` comment ends at the newline, full stop: a trailing `\` does NOT continue it
            // (upstream's `SkipComment` does; the 686975d reader never did, and Windows paths
            // end comments with a backslash: `// see C:\models\`). The newline stays for the
            // next round to see as whitespace.
            while src[*at..].chars().next().is_some_and(|c| c != '\n') {
                bump(src, at, line, col);
            }
            continue;
        }
        if c == '/' && second == Some('*') {
            let (start_line, start_col) = (*line, *col);
            bump(src, at, line, col);
            bump(src, at, line, col);
            loop {
                match src[*at..].chars().next() {
                    Some('*') if src[*at..].chars().nth(1) == Some('/') => {
                        bump(src, at, line, col);
                        bump(src, at, line, col);
                        break;
                    }
                    Some(_) => {
                        bump(src, at, line, col);
                    }
                    None => {
                        return Err(ParseError {
                            line: start_line,
                            col: start_col,
                            msg: "unterminated `/*` comment".to_string(),
                        })
                    }
                }
            }
            continue;
        }
        if c == '/' {
            return Err(ParseError {
                line: *line,
                col: *col,
                msg: "unexpected `/` (a comment is `//` or `/*`)".to_string(),
            });
        }
        return Ok(());
    }
}

/// Consume one character, keeping the 1-based line/col in step.
fn bump(src: &str, at: &mut usize, line: &mut usize, col: &mut usize) -> Option<char> {
    let c = src[*at..].chars().next()?;
    *at += c.len_utf8();
    if c == '\n' {
        *line += 1;
        *col = 1;
    } else {
        *col += 1;
    }
    Some(c)
}

pub(crate) struct Lexer<'a> {
    src: &'a str,
    /// Byte offset into `src`.
    at: usize,
    line: usize,
    col: usize,
    /// Read `"""` strings by upstream's rules instead of 686975d's (the retry mode).
    upstream_raw: bool,
}

impl<'a> Lexer<'a> {
    /// `line` is the 1-based line number of `src`'s first line (the header reader says how many
    /// lines it consumed).
    ///
    /// `upstream_raw` selects how `"""` strings end: `false` is 686975d's rule (first `"""`
    /// closes), `true` upstream's (see the module docs). The parser tries `false` first and
    /// only retries with `true` when the file did not parse, so a file 686975d accepted always
    /// reads as it did.
    pub(crate) fn new(src: &'a str, line: usize, upstream_raw: bool) -> Self {
        Self { src, at: 0, line, col: 1, upstream_raw }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError { line: self.line, col: self.col, msg: msg.into() }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.at..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.src[self.at..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        bump(self.src, &mut self.at, &mut self.line, &mut self.col)
    }

    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        skip_trivia(self.src, &mut self.at, &mut self.line, &mut self.col)
    }

    /// The next token, or `None` at end of input.
    pub(crate) fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        self.skip_trivia()?;
        let (line, col) = (self.line, self.col);
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(None),
        };
        let kind = match c {
            '{' => {
                self.bump();
                Kind::LBrace
            }
            '}' => {
                self.bump();
                Kind::RBrace
            }
            '[' => {
                self.bump();
                Kind::LBracket
            }
            ']' => {
                self.bump();
                Kind::RBracket
            }
            '=' => {
                self.bump();
                Kind::Eq
            }
            ',' => {
                self.bump();
                Kind::Comma
            }
            '#' => Kind::Blob(self.blob(line, col)?),
            '"' | '\'' => {
                if c == '"' && self.peek_at(1) == Some('"') && self.peek_at(2) == Some('"') {
                    Kind::Raw(if self.upstream_raw {
                        self.raw_string_upstream(line, col)?
                    } else {
                        self.raw_string(line, col)?
                    })
                } else {
                    Kind::Quoted(self.quoted_string(c, line, col)?)
                }
            }
            // All three terminate tokens (upstream `TokenTerminators`), so on their own they
            // make an empty token — an error (upstream's `unexpected character` case).
            ':' | '|' | ';' => return Err(self.err(format!("unexpected character {c:?}"))),
            c if is_unquoted_token_char(c) => self.unquoted_token()?,
            _ => return Err(self.err(format!("unexpected character {c:?}"))),
        };
        Ok(Some(Token { kind, line, col }))
    }

    /// An unquoted token run; when the token is an identifier and nothing but whitespace and
    /// comments separates it from a `:` or `|`, that separator is swallowed and the token is a
    /// flag (686975d looked through comments there too).
    ///
    /// The run is 686975d's: after a letter or `_` it is `[A-Za-z0-9_.]`; after anything else
    /// (a digit, `-`, `+`, `.`) it is `[A-Za-z0-9_.+-]`. Upstream's wider "any printable ASCII"
    /// run swallowed `/`, `#` and `-` (`1// c`, `true#[01]`, `null-5`), which 686975d read as
    /// separate tokens.
    fn unquoted_token(&mut self) -> Result<Kind, ParseError> {
        let start = self.at;
        let first = self.peek().unwrap_or(' ');
        if first.is_ascii_alphabetic() || first == '_' {
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
            {
                self.bump();
            }
        } else if first.is_ascii_alphanumeric() || matches!(first, '.' | '+' | '-') {
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
            {
                self.bump();
            }
        } else {
            return Err(self.err(format!("unexpected character {first:?}")));
        }
        let text = self.src[start..self.at].to_string();
        if crate::is_identifier(&text) {
            self.skip_trivia()?;
            if matches!(self.peek(), Some(':') | Some('|')) {
                self.bump();
                return Ok(Kind::Flag(text));
            }
        }
        Ok(Kind::Ident(text))
    }

    /// `# [ XX XX … ]` — the marker, then (whitespace and comments may sit anywhere) `[`, then
    /// hex bytes as run-together-or-separated nibbles, then `]` (commit 686975d's scanner with
    /// upstream's trivia skipping added — see the module docs).
    ///
    /// Odd nibble counts and non-hex characters are refused; the positions mirror 686975d (the
    /// `#[` for the file-span errors, the offending character for a bad nibble).
    fn blob(&mut self, line: usize, col: usize) -> Result<Vec<u8>, ParseError> {
        self.bump(); // the `#` marker
        if self.peek().is_some_and(is_unquoted_token_char) {
            return Err(ParseError {
                line,
                col,
                msg: "expected `#[` to open a blob".to_string(),
            });
        }
        self.skip_trivia()?;
        match self.peek() {
            Some('[') => {
                self.bump();
            }
            Some(c) => {
                return Err(ParseError {
                    line: self.line,
                    col: self.col,
                    msg: format!("expected `[` after `#` to open a blob, found {c:?}"),
                })
            }
            None => {
                return Err(ParseError {
                    line,
                    col,
                    msg: "unterminated `#[` blob".to_string(),
                })
            }
        }
        let mut out = Vec::new();
        let mut high: Option<u8> = None;
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(']') => {
                    self.bump();
                    break;
                }
                None => {
                    return Err(ParseError {
                        line,
                        col,
                        msg: "unterminated `#[` blob".to_string(),
                    })
                }
                Some(c) => {
                    let nibble = match c.to_digit(16) {
                        Some(nibble) => nibble as u8,
                        None => {
                            return Err(ParseError {
                                line: self.line,
                                col: self.col,
                                msg: format!("`{c}` is not a hex digit in a `#[` blob"),
                            })
                        }
                    };
                    self.bump();
                    match high.take() {
                        None => high = Some(nibble),
                        Some(h) => out.push((h << 4) | nibble),
                    }
                }
            }
        }
        if high.is_some() {
            return Err(ParseError {
                line,
                col,
                msg: "`#[` blob has an odd number of hex digits".to_string(),
            });
        }
        Ok(out)
    }

    /// `"…"` / `'…'`: the two behave alike (upstream `ReadQuotedStringRaw`). A backslash
    /// escapes the next character and the cs2-kv3 escape table applies (`\n`, `\t`, `\r`,
    /// `\"`, `\\`; any other `\X` keeps the backslash AND the character — see [`unescape`]);
    /// a real newline (or a `\` in front of one) is allowed inside and kept.
    fn quoted_string(&mut self, quote: char, line: usize, col: usize) -> Result<String, ParseError> {
        self.bump(); // opening quote
        let mut raw = String::new();
        let mut escaped = false;
        loop {
            let Some(c) = self.peek() else {
                return Err(ParseError { line, col, msg: "unterminated string".to_string() });
            };
            self.bump();
            if c == '\\' {
                escaped = !escaped;
                raw.push(c);
                continue;
            }
            if c == quote && !escaped {
                break;
            }
            escaped = false;
            raw.push(c);
        }
        if raw.contains('\\') {
            raw = unescape(&raw);
        }
        Ok(raw)
    }

    /// `"""` … `"""`, the 686975d rule: the string closes at the first `"""`, a `\r\n` or lone
    /// `\r` inside is one `\n`, and one leading and one trailing newline are dropped. Upstream
    /// demands a newline after the opener and a closing `"""` on a line of its own (and
    /// un-escapes `\"""`); this reader gave those up — the same bytes can mean something else
    /// to 686975d, and old wins.
    fn raw_string(&mut self, line: usize, col: usize) -> Result<String, ParseError> {
        self.bump();
        self.bump();
        self.bump();
        let mut out = String::new();
        loop {
            if self.peek() == Some('"')
                && self.peek_at(1) == Some('"')
                && self.peek_at(2) == Some('"')
            {
                self.bump();
                self.bump();
                self.bump();
                break;
            }
            match self.bump() {
                Some('\r') => {
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                    out.push('\n');
                }
                Some(c) => out.push(c),
                None => {
                    return Err(ParseError {
                        line,
                        col,
                        msg: "unterminated `\"\"\"` string".to_string(),
                    })
                }
            }
        }
        if let Some(rest) = out.strip_prefix('\n') {
            out = rest.to_string();
        }
        if let Some(rest) = out.strip_suffix('\n') {
            out = rest.to_string();
        }
        Ok(out)
    }

    /// The fallback form of a `"""` string (upstream `ReadMultilineStringContents`), only used when
    /// the whole file failed to parse with 686975d's rule (see [`Lexer::new`]): the opening quotes are followed
    /// by a newline (an optional `\r` first); the string ends at `"""` on a line of its own
    /// whose preceding newline is not itself backslash-escaped. Content is verbatim with
    /// CRLF normalised to LF, one leading and one trailing newline dropped, and `\"""`
    /// un-escaped to `"""`.
    fn raw_string_upstream(&mut self, line: usize, col: usize) -> Result<String, ParseError> {
        self.bump();
        self.bump();
        self.bump();
        match self.peek() {
            Some('\r') if self.peek_at(1) == Some('\n') => {
                self.bump();
                self.bump();
            }
            Some('\n') => {
                self.bump();
            }
            Some(c) => {
                return Err(ParseError {
                    line,
                    col,
                    msg: format!("a `\"\"\"` string must open with a newline, found {c:?}"),
                })
            }
            None => {
                return Err(ParseError {
                    line,
                    col,
                    msg: "unterminated `\"\"\"` string".to_string(),
                })
            }
        }
        // The virtual newline stands in for the one after the opening `"""`, so the closing
        // check sees an empty string the same way upstream's buffer does.
        let mut buf = String::from("\n");
        loop {
            if self.peek() == Some('"') && self.closes_here(&buf) {
                self.bump();
                self.bump();
                self.bump();
                break;
            }
            match self.bump() {
                Some(c) => buf.push(c),
                None => {
                    return Err(ParseError {
                        line,
                        col,
                        msg: "unterminated `\"\"\"` string".to_string(),
                    })
                }
            }
        }
        buf.remove(0); // the virtual newline after the opening `"""`
        let buf = buf.replace("\r\n", "\n");
        let mut buf = buf;
        buf.pop(); // the newline before the closing `"""`
        Ok(buf.replace("\\\"\"\"", "\"\"\""))
    }

    /// True when the next three characters are `"""` that close the string: they sit right
    /// after a newline (the virtual one counts, so the empty string closes immediately) and
    /// that newline is not itself backslash-escaped (upstream's `sb[^4] == '\n'` and
    /// `sb[^5] != '\\'` checks).
    fn closes_here(&self, buf: &str) -> bool {
        if self.peek() != Some('"') || self.peek_at(1) != Some('"') || self.peek_at(2) != Some('"') {
            return false;
        }
        if !buf.as_bytes().last().is_some_and(|b| *b == b'\n') {
            return false;
        }
        match buf.len() {
            1 => true,
            _ => buf.as_bytes()[buf.len() - 2] != b'\\',
        }
    }
}

/// The cs2-kv3 escape table, restored from commit 686975d: `\n` -> newline, `\t` -> tab,
/// `\r` -> carriage return (the writer emits `\r`, so a write->parse round trip is exact),
/// `\"` -> quote, `\\` -> backslash; any other `\X` keeps BOTH characters (`\x` stays `\x`, a
/// backslash before a real newline stays). Upstream's `UnescapeString` maps only `\n` and `\t`
/// and drops the backslash for everything else; the two disagree on values it decodes, and WP5
/// round 2 item 3 pins the pre-conformance behaviour where they differ. The raw form can never
/// end in a lone backslash (the escape toggling above consumes the character after one), but
/// the fallback stays safe regardless.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
