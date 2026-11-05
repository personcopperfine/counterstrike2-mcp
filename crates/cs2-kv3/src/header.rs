//! The `<!-- kv3 … -->` header, in two strictness levels.
//!
//! The header is a token stream, not a line: `<!--` `kv3` `encoding` `:` name `:` `version` `{`
//! GUID `}` `format` `:` name `:` `version` `{` GUID `}` `-->`, with whitespace and comments
//! allowed between any two tokens and every keyword compared case-insensitively (so multi-line
//! and commented headers parse, like upstream's token reader). In **strict** mode
//! ([`Header::parse_strict`], [`Header::validate_strict`]) the encoding name must be `text`,
//! both version GUIDs must be GUIDs, the encoding GUID must be the text-encoding GUID, and a
//! `generic` format GUID must be the generic one — exactly ValveKeyValue's
//! `KV3TokenReader.ReadHeader`. In **lenient** mode ([`parse`](crate::parse) and
//! [`Header::parse`]) the 686975d first-line reader runs *first* and wins whenever it accepts
//! the line (its `raw`, `encoding` and `format` are kept verbatim, quirks included); only when
//! it refuses does the token scan run, without the GUID checks but still requiring the encoding
//! to be `text` (case-insensitively). The union is therefore: everything 686975d accepted, plus
//! upstream's multi-line / commented / case-varied headers.
//!
//! Differences from upstream, lenient mode only: the version GUIDs keep any spelling (686975d
//! accepted `version{x}`), the format GUID is unchecked, and malformed one-line headers (a
//! swapped field order, extra tokens, odd `version:` spellings) stay acceptable through the
//! 686975d reader. Strict mode has none of these.

use crate::lexer::skip_trivia;
use crate::{ENCODING_TEXT_GUID, FORMAT_GENERIC_GUID, Header, ParseError, is_identifier};

impl Header {
    /// Parse the header at the start of `text` — the same lenient reader [`parse`](crate::parse)
    /// uses, so it accepts what the 686975d `Header::parse` accepted and nothing less (callers
    /// like hammer-mcp's `vpcf/template.rs` hand it one header line). Like 686975d it refuses
    /// anything but spaces after the `-->` on the header's last line; a body may follow on the
    /// next line. For upstream's exact checks use [`Header::validate_strict`] or
    /// [`Header::parse_strict`].
    pub fn parse(text: &str) -> Result<Header, ParseError> {
        let (header, consumed, _) = read_header(text, false)?;
        // The 686975d reader consumed the whole first line (and has already checked that it ends
        // in `-->`); the token scan stops right after the `-->`, so look at what is left of
        // that line.
        let rest = if text[..consumed].ends_with('\n') { "" } else { &text[consumed..] };
        let line_rest = rest.split('\n').next().unwrap_or("");
        if !line_rest.trim_matches(['\r', ' ', '\t']).is_empty() {
            return Err(ParseError {
                line: 1,
                col: 1,
                msg: format!("not a kv3 header comment: {:?} follows the `-->`", line_rest.trim()),
            });
        }
        Ok(header)
    }

    /// Parse the header at the start of `text` with upstream's full checks (the token-stream
    /// shape, the `text` encoding and both GUIDs), ignoring whatever follows the `-->`.
    pub fn parse_strict(text: &str) -> Result<Header, ParseError> {
        read_header(text, true).map(|(header, _, _)| header)
    }

    /// Re-check an already-parsed header against upstream's exact rules (the token-stream shape,
    /// the `text` encoding, the two version GUIDs): [`parse`](crate::parse) is the documented
    /// 686975d-∪-upstream union and may accept headers upstream refuses (`version{x}`, a wrong
    /// encoding GUID, a swapped field order …), so conformance tests that need ValveKeyValue's
    /// accept/reject behaviour assert through this instead.
    ///
    /// The token scan runs again over [`Header::raw`] in strict mode, so a leniently accepted
    /// header reports the upstream error (the `kv3 header:` prefix, positions relative to the
    /// header itself).
    pub fn validate_strict(&self) -> Result<(), ParseError> {
        read_header(&self.raw, true).map(|_| ())
    }
}

/// What one `encoding:`/`format:` field contributes: its name and its `version{…}` contents.
struct Field {
    name: String,
    version: String,
}

/// Read the header from the start of `text`; returns the header, the byte length it consumed
/// (up to and including the `-->`) and how many `\n` that span contains (the body lexer starts
/// on the following line).
///
/// The token scan runs first. In strict mode its failures and the upstream field checks are
/// returned as-is; in lenient mode a failed scan falls back to the 686975d first-line reader so
/// the union of accepted inputs is kept (and a scan failure the fallback also rejects keeps the
/// scan's more specific message).
pub(crate) fn read_header(text: &str, strict: bool) -> Result<(Header, usize, usize), ParseError> {
    // Lenient mode asks the 686975d first-line reader first and keeps its answer whenever it
    // accepts: where the two readers disagree about a line, old wins (a `-->` repeated in the
    // line, an empty `format:` name, a swapped field order all stay what 686975d made of them).
    if !strict {
        if let Ok(header) = fallback_header(text) {
            return Ok((header, fallback_body_start(text), 1));
        }
    }
    let mut scan = Scan { text, at: 0, line: 1, col: 1 };
    let start_at = (scan.line, scan.col);
    scan.ws_and_comments()?;
    let start = scan.at;

    let (encoding, format, at) = scan.header(strict)?;
    if strict {
        validate(&encoding, &format, start_at)?;
    } else if !encoding.name.eq_ignore_ascii_case("text") {
        // cs2-kv3 reads text kv3 only (686975d refused every other encoding too).
        return Err(ParseError {
            line: start_at.0,
            col: start_at.1,
            msg: format!(
                "kv3 header: unsupported encoding {:?} (cs2-kv3 reads text kv3 only)",
                encoding.name
            ),
        });
    }
    let raw = text[start..at].to_string();
    let lines = text[..at].bytes().filter(|byte| *byte == b'\n').count();
    Ok((Header { raw, encoding: encoding.name, format: format.name }, at, lines))
}

/// Upstream `KV3TokenReader.ReadHeader`'s post-read checks (the errors point at the start of the
/// header, hence every header error mentions the context).
fn validate(encoding: &Field, format: &Field, start_at: (usize, usize)) -> Result<(), ParseError> {
    let err = |msg: String| ParseError { line: start_at.0, col: start_at.1, msg };
    if !encoding.name.eq_ignore_ascii_case("text") {
        return Err(err(format!(
            "kv3 header: unrecognized encoding `{}`, expected `text`",
            encoding.name
        )));
    }
    if !guid_eq(&encoding.version, ENCODING_TEXT_GUID) {
        return Err(err(format!(
            "kv3 header: unrecognized encoding version, expected `{ENCODING_TEXT_GUID}` but got `{}`",
            encoding.version
        )));
    }
    if format.name.eq_ignore_ascii_case("generic")
        && !guid_eq(&format.version, FORMAT_GENERIC_GUID)
    {
        return Err(err(format!(
            "kv3 header: unrecognized format version, expected `{FORMAT_GENERIC_GUID}` but got `{}`",
            format.version
        )));
    }
    Ok(())
}

/// The byte offset of the body the 686975d reader left after the first line: everything up to
/// and including the first `\n`, or the whole text when there is none (the lexer then sees an
/// empty body, like 686975d's did).
fn fallback_body_start(text: &str) -> usize {
    match text.find('\n') {
        Some(at) => at + 1,
        None => text.len(),
    }
}

/// The pre-conformance header reader (commit 686975d), verbatim, as the union's fallback: any
/// `<!-- … -->` line whose whitespace-separated fields include `kv3`, an `encoding:<name>:…`
/// field whose value before the next colon is `text`, and a `format:<name>:…` field. The field
/// order, the `version{…}` contents, extra fields and extra field segments it tolerated stay
/// tolerated. This only ever runs on the first line ([`fallback_body_start`]), exactly where
/// 686975d looked.
fn fallback_header(text: &str) -> Result<Header, ParseError> {
    let line = match text.find('\n') {
        Some(at) => &text[..at],
        None => text,
    };
    let raw = line.trim_end_matches(['\r', '\n', ' ', '\t']).to_string();
    let err = |msg: String| ParseError { line: 1, col: 1, msg };
    if !raw.starts_with("<!--") || !raw.ends_with("-->") {
        return Err(err(format!("not a kv3 header comment: {raw:?}")));
    }
    let inner = raw
        .trim_start_matches("<!--")
        .trim_end_matches("-->")
        .trim();
    let mut fields = inner.split_whitespace();
    if fields.next() != Some("kv3") {
        return Err(err(format!("header does not start with `kv3`: {raw:?}")));
    }
    let encoding = fields
        .clone()
        .find_map(|f| f.strip_prefix("encoding:"))
        .and_then(|v| v.split(':').next())
        .ok_or_else(|| err(format!("header has no `encoding:` field: {raw:?}")))?
        .to_string();
    let format = fields
        .find_map(|f| f.strip_prefix("format:"))
        .and_then(|v| v.split(':').next())
        .ok_or_else(|| err(format!("header has no `format:` field: {raw:?}")))?
        .to_string();
    if encoding != "text" {
        return Err(err(format!(
            "unsupported kv3 encoding {encoding:?} (cs2-kv3 reads text kv3 only)"
        )));
    }
    Ok(Header { raw, encoding, format })
}

/// `Guid.TryParse` for the two forms that can appear between the braces: the hyphenated
/// `8-4-4-4-12` form and the bare 32-hex-digit form (a `Guid` compares by value, so both spell
/// the same GUID).
fn is_guid(text: &str) -> bool {
    let hex = |part: &str| part.bytes().all(|b| (b as char).is_ascii_hexdigit());
    let parts: Vec<&str> = text.split('-').collect();
    if let [a, b, c, d, e] = parts[..] {
        [(8, a), (4, b), (4, c), (4, d), (12, e)]
            .iter()
            .all(|(len, part)| part.len() == *len && hex(part))
    } else {
        text.len() == 32 && hex(text)
    }
}

/// GUID equality: case-insensitive, ignoring hyphenation (a `Guid` compares by value).
fn guid_eq(text: &str, expected: &str) -> bool {
    let dense = |text: &str| text.to_ascii_lowercase().replace('-', "");
    dense(text) == dense(expected)
}

/// The token scanner for one header (same character and trivia rules as the body lexer).
struct Scan<'a> {
    text: &'a str,
    /// Byte offset into `text`.
    at: usize,
    line: usize,
    col: usize,
}

impl<'a> Scan<'a> {
    fn err_at(&self, at: (usize, usize), msg: impl Into<String>) -> ParseError {
        ParseError { line: at.0, col: at.1, msg: msg.into() }
    }

    fn ws_and_comments(&mut self) -> Result<(), ParseError> {
        skip_trivia(self.text, &mut self.at, &mut self.line, &mut self.col)
    }

    /// One unquoted header token: printable ASCII outside `{}[]=,'":|;` (upstream
    /// `ReadUnquotedToken`; header tokens are never quoted).
    fn token(&mut self) -> String {
        let start = self.at;
        while self
            .peek()
            .is_some_and(crate::lexer::is_unquoted_token_char)
        {
            self.bump();
        }
        self.text[start..self.at].to_string()
    }

    /// Consume one token and require it to equal `expected`, case-insensitively (upstream
    /// `ExpectHeaderToken`); the error points at the token, after leading trivia.
    fn expect_token(&mut self, expected: &str) -> Result<(), ParseError> {
        self.ws_and_comments()?;
        let at = (self.line, self.col);
        let got = self.token();
        if got.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(self.err_at(at, format!("kv3 header: expected `{expected}` but got `{got}`")))
        }
    }

    /// Consume one specific character (whitespace/comments may sit in front of it), like
    /// upstream's `ExpectHeaderChar`; the error points at the offending character.
    fn expect_char(&mut self, expected: char) -> Result<(), ParseError> {
        self.ws_and_comments()?;
        let at = (self.line, self.col);
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(
                self.err_at(at, format!("kv3 header: expected `{expected}` but got `{c}`"))
            ),
            None => Err(self.err_at(
                at,
                format!("kv3 header: found end of file where `{expected}` was expected"),
            )),
        }
    }

    /// A `encoding`/`format` field: `kw : name : version { … }`. The kw must be the literal
    /// keyword, the name an identifier; the version contents must be a GUID in strict mode
    /// (upstream `ReadVersionGuid`) and may be anything in lenient mode (686975d never looked
    /// inside `version{…}` — `version{ "gid" }}` one-liners reach that through the fallback).
    fn field(&mut self, keyword: &str, strict: bool) -> Result<Field, ParseError> {
        self.expect_token(keyword)?;
        self.expect_char(':')?;
        let name = self.read_name()?;
        self.expect_char(':')?;
        self.expect_token("version")?;
        self.expect_char('{')?;
        self.ws_and_comments()?;
        let at = (self.line, self.col);
        let version = self.token();
        if strict && !is_guid(&version) {
            return Err(
                self.err_at(at, format!("kv3 header: expected a version GUID but got `{version}`"))
            );
        }
        self.expect_char('}')?;
        Ok(Field { name, version })
    }

    /// A header name (`encoding`/`format`): an unquoted token that must be an identifier.
    fn read_name(&mut self) -> Result<String, ParseError> {
        self.ws_and_comments()?;
        let at = (self.line, self.col);
        let name = self.token();
        if !is_identifier(&name) {
            return Err(self.err_at(at, format!("kv3 header: expected a name but got `{name}`")));
        }
        Ok(name)
    }

    /// The whole headed token stream: the two fields, plus where the `-->` ended.
    fn header(&mut self, strict: bool) -> Result<(Field, Field, usize), ParseError> {
        self.expect_token("<!--")?;
        self.expect_token("kv3")?;
        let encoding = self.field("encoding", strict)?;
        let format = self.field("format", strict)?;
        self.expect_token("-->")?;
        Ok((encoding, format, self.at))
    }

    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.text[self.at..].chars().next()?;
        self.at += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }
}
