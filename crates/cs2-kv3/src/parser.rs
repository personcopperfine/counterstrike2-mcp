//! Recursive-descent parser: tokens -> [`Document`].

use crate::lexer::{Kind, Lexer, Token};
use crate::{Document, Key, Object, ParseError, Value, MAX_DEPTH};

/// Parse a whole KV3 text file (header + one object).
///
/// Comments are skipped, line endings are not part of the tree; see the crate docs for the
/// format's grammar and for the two layouts `write` normalises.
pub fn parse(text: &str) -> Result<Document, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (header, consumed, lines) = crate::header::read_header(text, false)?;
    let body = &text[consumed..];

    // `"""` strings are lexed by 686975d's rule first; a file that fails that way gets one more
    // try with upstream's (see `Lexer::new`), and the first error stands when both fail.
    match parse_body(body, 1 + lines, false) {
        Ok(root) => Ok(Document { header, root }),
        Err(first) if body.contains("\"\"\"") => match parse_body(body, 1 + lines, true) {
            Ok(root) => Ok(Document { header, root }),
            Err(_) => Err(first),
        },
        Err(first) => Err(first),
    }
}

fn parse_body(body: &str, first_line: usize, upstream_raw: bool) -> Result<Object, ParseError> {
    let mut lexer = Lexer::new(body, first_line, upstream_raw);
    let mut tokens = Vec::new();
    while let Some(token) = lexer.next_token()? {
        tokens.push(token);
    }

    let mut parser = Parser { tokens, at: 0, depth: 0 };
    let root = parser.root_object()?;
    if let Some(extra) = parser.peek() {
        return Err(ParseError {
            line: extra.line,
            col: extra.col,
            msg: format!("trailing {} after the top-level object", extra.what()),
        });
    }
    Ok(root)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    /// Open nesting levels: objects, arrays and `flag:` chains together, the top-level object
    /// included (so it is 1 while the root object is being read). Never above [`MAX_DEPTH`].
    depth: usize,
}

// Flag names: any identifier is accepted as a flag and kept verbatim, like the pre-conformance
// reader did (WP5 round 2 item 4). Upstream's `KV3TextReader.ParseFlag` knows exactly seven
// canonical names — `resource`, `resource_name`, `panorama`, `soundevent`, `subclass`,
// `entity_name`, `localize` — and refuses anything else (`Unknown flag '…'`); this reader keeps
// the name instead so hand-written and tool-written bodies keep parsing, and none of the seven
// names is a special case here (they are all just identifiers).
impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        if token.is_some() {
            self.at += 1;
        }
        token
    }

    fn position(&self) -> (usize, usize) {
        match self.peek() {
            Some(t) => (t.line, t.col),
            None => match self.tokens.last() {
                Some(t) => (t.line, t.col + 1),
                None => (2, 1),
            },
        }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        let (line, col) = self.position();
        ParseError { line, col, msg: msg.into() }
    }

    fn describe(&self) -> String {
        match self.peek() {
            Some(t) => t.what(),
            None => "end of file".to_string(),
        }
    }

    fn expect(&mut self, kind: Kind, what: &str) -> Result<Token, ParseError> {
        let matches = self.peek().map(|t| t.kind == kind).unwrap_or(false);
        if matches {
            return Ok(self.bump().expect("peeked"));
        }
        Err(self.err(format!("expected {what}, found {}", self.describe())))
    }

    /// Count one more open nesting level and refuse the one that would be deeper than
    /// [`MAX_DEPTH`], so the recursion below is bounded.
    ///
    /// The error points at the opening token (still the current one): the `{` / `[`, or the
    /// flag of a `flag:` chain.
    fn enter(&mut self) -> Result<(), ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(self.err(format!("nesting deeper than {MAX_DEPTH} levels")));
        }
        self.depth += 1;
        Ok(())
    }

    /// The one top-level object of a file: nesting level 1.
    fn root_object(&mut self) -> Result<Object, ParseError> {
        self.enter()?;
        if !self.peek().is_some_and(|t| t.kind == Kind::LBrace) {
            // Upstream accepts any root value (`RootTypesTestCase`); this reader's `Document`
            // holds an object root and is frozen that way (hammer-mcp depends on it) — see the
            // crate docs' limitations.
            return Err(self.err(format!(
                "expected `{{` to open the root object (other root types are not supported by this reader), found {}",
                self.describe()
            )));
        }
        let object = self.object();
        self.depth -= 1;
        object
    }

    fn object(&mut self) -> Result<Object, ParseError> {
        self.expect(Kind::LBrace, "`{`")?;
        let mut object = Object::new();
        loop {
            // Commas between entries are optional — the 686975d reader skipped them wherever
            // they sit (`{ a = 1, b = 2 }`, `{ a = 1, }`, even a leading `{ , a = 1 }`); upstream
            // refuses them outside arrays, and the reader keeps the pre-conformance acceptance
            // (WP5 round 2 item 1).
            while self.peek().is_some_and(|t| t.kind == Kind::Comma) {
                self.bump();
            }
            if self.peek().is_some_and(|t| t.kind == Kind::RBrace) {
                self.bump();
                return Ok(object);
            }
            let Some(token) = self.peek().cloned() else {
                return Err(self.err("expected a key or `}` to close the object"));
            };
            let key = match token.kind {
                Kind::Ident(text) if crate::is_identifier(&text) => {
                    // A bare `.y` reads fine but is never written bare (see `is_plain_key`).
                    let quoted = !crate::is_plain_key(&text);
                    Key { text, quoted }
                }
                Kind::Ident(text) => {
                    return Err(ParseError {
                        line: token.line,
                        col: token.col,
                        msg: format!(
                            "invalid key `{text}`: keys that are not identifiers must be quoted"
                        ),
                    })
                }
                Kind::Quoted(text) => Key { text, quoted: true },
                // A `"""` key is written back quoted (a raw string cannot be an output form).
                Kind::Raw(text) => Key { text, quoted: true },
                _ => {
                    return Err(ParseError {
                        line: token.line,
                        col: token.col,
                        msg: format!("expected a key, found {}", token.what()),
                    })
                }
            };
            self.bump();
            self.expect(Kind::Eq, "`=` after the key")?;
            let value = self.value()?;
            object.insert_full(key, value);
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| self.err("expected a value"))?;
        let value = match token.kind {
            Kind::Ident(ref word) => {
                let lower = word.to_ascii_lowercase();
                match lower.as_str() {
                    // The literals are case-insensitive (upstream `ParseValue`).
                    "null" => Value::Null,
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    // The float literals keep their source spelling (`+inf` stays `+inf`).
                    "nan" | "inf" | "+inf" | "-inf" => {
                        Value::Number(crate::Number::parse(word).expect("checked a float literal"))
                    }
                    _ => {
                        if word == "+" {
                            // A lone plus is read as zero (upstream `ParseValue`); normalised
                            // to the `0` lexeme so accessors see the value upstream sees.
                            Value::Number(crate::Number::from_i64(0))
                        } else if let Some(number) = crate::Number::parse(word) {
                            Value::Number(number)
                        } else {
                            return Err(ParseError {
                                line: token.line,
                                col: token.col,
                                msg: format!(
                                    "`{word}` is not a value (a flag is `flag:value`, strings must be quoted)"
                                ),
                            });
                        }
                    }
                }
            }
            Kind::Flag(ref name) => {
                // `flag: value` and `flag| value` are the same (the lexer consumes either). A
                // chain nests (`a:b:1` is `a` flagging `b:1`), exactly like 686975d: upstream's
                // state machine keeps only the last flag, which would lose data on a re-write.
                // Any identifier is a flag name, kept verbatim (686975d; upstream's `ParseFlag`
                // refuses unknown names — see `KNOWN_FLAGS` and the crate docs).
                self.enter()?;
                let flag = name.clone();
                self.bump();
                let inner = self.value();
                self.depth -= 1;
                return Ok(Value::Flagged { flag, value: Box::new(inner?) });
            }
            Kind::Quoted(ref text) => Value::String(text.clone()),
            Kind::Raw(ref text) => Value::String(text.clone()),
            Kind::Blob(ref bytes) => Value::Blob(bytes.clone()),
            Kind::LBrace => {
                self.enter()?;
                let object = self.object();
                self.depth -= 1;
                return Ok(Value::Object(object?));
            }
            Kind::LBracket => {
                self.enter()?;
                let array = self.array();
                self.depth -= 1;
                return Ok(Value::Array(array?));
            }
            _ => {
                return Err(ParseError {
                    line: token.line,
                    col: token.col,
                    msg: format!("expected a value, found {}", token.what()),
                })
            }
        };
        self.bump();
        Ok(value)
    }

    fn array(&mut self) -> Result<Vec<Value>, ParseError> {
        self.expect(Kind::LBracket, "`[`")?;
        let mut items = Vec::new();
        loop {
            // Commas are optional between elements: `[ 1 2 ]`, a trailing `[ 1, 2, ]` and even a
            // leading `[ , 1 ]` all parsed with the pre-conformance reader (checked against
            // 686975d), so the reader keeps accepting them (WP5 round 2 item 2). Upstream
            // requires a comma between elements and refuses a leading one.
            while self.peek().is_some_and(|t| t.kind == Kind::Comma) {
                self.bump();
            }
            if self.peek().is_some_and(|t| t.kind == Kind::RBracket) {
                self.bump();
                return Ok(items);
            }
            match self.peek() {
                None => return Err(self.err("expected `]` to close the array")),
                Some(token) if token.kind == Kind::RBrace => {
                    return Err(ParseError {
                        line: token.line,
                        col: token.col,
                        msg: "expected `]` to close the array, found `}`".to_string(),
                    })
                }
                _ => {}
            }
            items.push(self.value()?);
        }
    }
}
