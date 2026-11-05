//! Port of upstream `KVTokenReader` (position tracking, peek/next, whitespace) and
//! `KV1TokenReader` (the KV1 text grammar): quoted and unquoted strings, `=`, `{`, `}`,
//! `//` and `/` comments, and `[$CONDITIONS]`.
//!
//! The tokenizer works on the whole input `&str` as a `Vec<char>` (one pass, four bytes per
//! character at worst), so peek/next are O(1) and one character is one column. Upstream counts
//! UTF-16 code units, so a column differs after an astral character (see the reader's
//! `# Differences from upstream`). Positions are 1-based;
//! a `\n` resets the column and is not counted itself (upstream counts `\r` as an ordinary
//! column character, and so does this port).
//!
//! # Differences from upstream
//!
//! * **A leading UTF-8 BOM is skipped and does not count towards the column.** Upstream reads
//!   through a `StreamReader` with byte-order-mark detection, which eats a BOM before the
//!   reader ever sees a character; here the input is already a `&str`, so the BOM is dropped
//!   in [`Tokenizer::new`] the way the `StreamReader` would have dropped it. Valve's files do
//!   carry a BOM (every fixture that starts with `M-oM-;M-?` upstream does).
//! * **Tokens carry their own position** ([`Token::line`] / [`Token::column`]). Upstream keeps
//!   the position of the last read token on the reader (`TokenStartPosition`) and the reader
//!   side reads it from there; passing it in the token keeps the same information while a
//!   token is alive, and the reader still reports exactly the position upstream reports.
//! * **Errors are [`Error::Syntax`] with the position in the fields** rather than a
//!   `KeyValueException` whose message embeds `at line …`, `column …`; the message text is the
//!   upstream text with that clause removed. Everything else — which inputs are accepted and
//!   which are rejected — is unchanged.

use crate::Error;

const QUOTATION_MARK: char = '"';
const OBJECT_START: char = '{';
const OBJECT_END: char = '}';
/// Although Valve uses the double-slash convention, the KV spec allows single-slash comments.
const COMMENT_BEGIN: char = '/';
const CONDITION_BEGIN: char = '[';
const CONDITION_END: char = ']';
const ASSIGNMENT: char = '=';

/// The byte-order mark a `StreamReader` would have consumed before the reader could see it.
const BOM: char = '\u{feff}';

/// Upstream `KVTokenType`, minus the KV3-specific members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    /// `{`.
    ObjectStart,
    /// `}`.
    ObjectEnd,
    /// A quoted or unquoted character run. The reader re-classifies it into
    /// [`TokenKind::Key`] (or an include directive) from its own state, like upstream
    /// `KV1TextReader.ClassifyToken`.
    String,
    /// A character run in key position: at the start of an object or right after a value. Only
    /// the reader knows which side of the assignment a run sits on.
    Key,
    /// `=`, between a key and its value. The reader turns it back into a
    /// [`TokenKind::String`] anywhere else.
    Assignment,
    /// A comment; its text is dropped (KV1 comments are not part of the data model).
    Comment,
    /// `[$CONDITIONS]`, brackets removed.
    Condition,
    /// The string `#include` (any case) at document level.
    IncludeAndAppend,
    /// The string `#base` (any case) at document level.
    IncludeAndMerge,
    /// End of input.
    EndOfFile,
}

/// One KV1 text token (upstream `KVToken`), with the position of its first character.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    /// The token's text, for [`TokenKind::String`], [`TokenKind::Key`],
    /// [`TokenKind::Assignment`] and [`TokenKind::Condition`].
    pub(crate) value: String,
    /// 1-based line of the token's first character.
    pub(crate) line: usize,
    /// 1-based column of the token's first character.
    pub(crate) column: usize,
}

/// Upstream `KV1TokenReader` plus the position tracking of `KVTokenReader`.
pub(crate) struct Tokenizer {
    chars: Vec<char>,
    index: usize,
    /// Count of `\n` characters consumed so far; the next character's line is `line + 1`.
    line: usize,
    /// Count of characters consumed on the current line; the next character's column is
    /// `column + 1`.
    column: usize,
    token_start_line: usize,
    token_start_column: usize,
    /// Whether a backslash-quote (`\"`) was read while escape sequences were disabled. If
    /// parsing subsequently fails, that is the likely cause and the error message points at
    /// the option (upstream `encounteredPossibleEscapeSequence`).
    possible_escape_sequence: bool,
    /// [`crate::text::ReadOptions::escapes`] — translate `\n`, `\t`, … inside quoted strings.
    escapes: bool,
    /// [`crate::text::ReadOptions::valve_null_byte_bug`] — an invalid escape becomes a NUL byte
    /// that truncates the string instead of being an error.
    valve_null_byte_bug: bool,
}

impl Tokenizer {
    /// Wraps `text` in a tokenizer. `text` is expected to be a whole document (or a whole
    /// included file); only the very first character may be a BOM.
    pub(crate) fn new(text: &str, escapes: bool, valve_null_byte_bug: bool) -> Self {
        let mut chars: Vec<char> = text.chars().collect();
        // The BOM is an encoding mark, not content: upstream's StreamReader consumes it before
        // the reader sees it, so it must not shift the first line's columns either.
        if chars.first() == Some(&BOM) {
            chars.remove(0);
        }

        Self {
            chars,
            index: 0,
            line: 0,
            column: 0,
            token_start_line: 1,
            token_start_column: 1,
            possible_escape_sequence: false,
            escapes,
            valve_null_byte_bug,
        }
    }

    /// 1-based line/column of the next character to be consumed.
    fn position(&self) -> (usize, usize) {
        (self.line + 1, self.column + 1)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    /// Consumes one character and advances line/column. Returns `None` at end of input.
    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.index += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    /// Consumes one character the caller has already peeked, without a panic path.
    fn bump(&mut self) {
        self.index += 1;
        if self.chars[self.index - 1] == '\n' {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }
    }

    /// The hint appended to syntax errors once a stray backslash-quote was read with escape
    /// sequences disabled (upstream `KV1TokenReader.MakeSyntaxException`). The option name in
    /// the text is upstream's C# one; it is kept so ported test expectations still match.
    pub(crate) fn escape_hint(&self) -> &'static str {
        if self.possible_escape_sequence {
            " A backslash-escaped quotation mark (\\\") was read, but escape sequences are \
             disabled - consider enabling KVSerializerOptions.HasEscapeSequences."
        } else {
            ""
        }
    }

    /// Reads the next token. Reading past the end keeps returning [`TokenKind::EndOfFile`],
    /// the way upstream's reader keeps returning an empty token.
    pub(crate) fn read_next_token(&mut self) -> Result<Token, Error> {
        self.swallow_whitespace();
        (self.token_start_line, self.token_start_column) = self.position();
        self.read_next_token_inner()
    }

    fn token(&self, kind: TokenKind, value: impl Into<String>) -> Token {
        Token {
            kind,
            value: value.into(),
            line: self.token_start_line,
            column: self.token_start_column,
        }
    }

    /// End-of-file while reading the *current* token: upstream lets `Next()` throw an
    /// `EndOfStreamException` and the reader turns it into this message, pointing at where the
    /// token started rather than where the input ended.
    fn eof_error(&self) -> Error {
        Error::Syntax {
            line: self.token_start_line,
            column: self.token_start_column,
            message: format!("Found end of file while trying to read the token.{}", self.escape_hint()),
        }
    }

    fn read_next_token_inner(&mut self) -> Result<Token, Error> {
        let Some(next_char) = self.peek() else {
            return Ok(self.token(TokenKind::EndOfFile, ""));
        };

        match next_char {
            OBJECT_START => {
                self.bump();
                Ok(self.token(TokenKind::ObjectStart, ""))
            }
            OBJECT_END => {
                self.bump();
                Ok(self.token(TokenKind::ObjectEnd, ""))
            }
            COMMENT_BEGIN => Ok(self.read_comment()),
            CONDITION_BEGIN => self.read_condition(),
            ASSIGNMENT => {
                self.bump();
                Ok(self.token(TokenKind::Assignment, "="))
            }
            QUOTATION_MARK => {
                let text = self.read_quoted_string()?;
                Ok(self.token(TokenKind::String, text))
            }
            _ => {
                let text = self.read_unquoted_string();
                Ok(self.token(TokenKind::String, text))
            }
        }
    }

    /// Upstream `ReadComment`: consumes `/`, then an optional second `/` that is part of the
    /// marker, then everything up to (not including) the next `\n`. The comment's text is not
    /// kept — the data model has no place for it and upstream's KV1 builder drops it too — and
    /// the line break is left as whitespace between tokens. A `\r` before the line break is
    /// consumed as part of the comment, exactly like upstream.
    fn read_comment(&mut self) -> Token {
        self.bump();
        if self.peek() == Some(COMMENT_BEGIN) {
            self.bump();
        }
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump();
        }
        self.token(TokenKind::Comment, "")
    }

    /// Upstream `ReadCondition`: the text between `[` and `]`, verbatim — conditionals are
    /// never quoted, so no escape sequence is translated inside them.
    fn read_condition(&mut self) -> Result<Token, Error> {
        self.bump();

        let mut text = String::new();
        while self.peek() != Some(CONDITION_END) {
            let Some(c) = self.next() else {
                return Err(self.eof_error());
            };
            text.push(c);
        }
        self.bump();

        Ok(self.token(TokenKind::Condition, text))
    }

    fn read_quoted_string(&mut self) -> Result<String, Error> {
        self.read_char(QUOTATION_MARK)?;
        let text = self.read_until(|c| c == QUOTATION_MARK)?;
        self.read_char(QUOTATION_MARK)?;
        Ok(text)
    }

    /// Upstream `ReadChar`: consumes `expected_char` or fails at the position of the character
    /// that was found instead. Running out of input is the caller-visible end-of-file error.
    fn read_char(&mut self, expected_char: char) -> Result<(), Error> {
        let (line, column) = self.position();
        match self.peek() {
            Some(found) if found == expected_char => {
                self.bump();
                Ok(())
            }
            Some(found) => Err(Error::Syntax {
                line,
                column,
                message: format!("The syntax is incorrect, expected '{expected_char}' but got '{found}'."),
            }),
            None => Err(self.eof_error()),
        }
    }

    /// Upstream `ReadUnquotedString`: a run that ends at whitespace, a quotation mark, a
    /// brace, `=`, or the end of input. A comment marker does not end it.
    fn read_unquoted_string(&mut self) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace()
                || c == QUOTATION_MARK
                || c == OBJECT_START
                || c == OBJECT_END
                || c == ASSIGNMENT
            {
                break;
            }
            text.push(c);
            self.bump();
        }
        text
    }

    fn swallow_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    /// Upstream `ReadUntil`: the body of a quoted string, up to (excluding) the terminator.
    ///
    /// With [`crate::text::ReadOptions::escapes`] a `\` starts an escape (`\n \t \v \b \r \f
    /// \a \\ ? \' \"`); an unknown one is an error at the backslash's own position, or — with
    /// the Valve null-byte bug on — a NUL character. Without escapes a backslash is literal,
    /// but one directly in front of a quotation mark latches `possible_escape_sequence` so a
    /// later syntax error can point at the option.
    fn read_until(&mut self, is_terminator: impl Fn(char) -> bool) -> Result<String, Error> {
        let mut text = String::new();
        let mut escape_next = false;
        // Position of the backslash that opened the escape whose character is being read: the
        // error for an unknown escape points there, like upstream.
        let mut escape_line = 0;
        let mut escape_column = 0;

        while escape_next || !self.peek().is_some_and(&is_terminator) {
            let Some(mut next) = self.next() else {
                return Err(self.eof_error());
            };

            if self.escapes {
                if !escape_next && next == '\\' {
                    escape_next = true;
                    // Only the backslash has been consumed, so it sits one column back on this
                    // line (upstream's math).
                    (escape_line, escape_column) = self.position();
                    escape_column -= 1;
                    continue;
                }

                if escape_next {
                    next = match next {
                        'n' => '\n',
                        't' => '\t',
                        'v' => '\u{000b}',
                        'b' => '\u{0008}',
                        'r' => '\r',
                        'f' => '\u{000c}',
                        'a' => '\u{0007}',
                        '\\' => '\\',
                        '?' => '?',
                        '\'' => '\'',
                        '"' => '"',
                        _ if self.valve_null_byte_bug => '\0',
                        other => {
                            return Err(Error::Syntax {
                                line: escape_line,
                                column: escape_column,
                                message: format!(
                                    "Unknown escape sequence '\\{other}'.{}",
                                    self.escape_hint()
                                ),
                            });
                        }
                    };
                    escape_next = false;
                }
            } else if next == '\\' && self.peek() == Some(QUOTATION_MARK) {
                self.possible_escape_sequence = true;
            }

            text.push(next);
        }

        // Valve bug-for-bug compatibility with tier1 KeyValues/CUtlBuffer: an invalid escape
        // sequence is a NUL byte, and the text is cut at it. Upstream applies this to every
        // `ReadUntil` result whenever the option is on — escapes on or off — so a raw NUL in
        // the input truncates too.
        if self.valve_null_byte_bug {
            if let Some(nul) = text.find('\0') {
                text.truncate(nul);
            }
        }

        Ok(text)
    }
}
