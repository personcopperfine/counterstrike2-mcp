//! Tokenizer for FGD files. Hand-written (no regex): FGD tokens are directives (`@PointClass`),
//! bare words (`spawnflags`, `-1`, `local.origin`), quoted strings, raw `(...)`/`{...}` spans,
//! and the punctuation `= : + [ ]`. Comments are `//` to end of line (never inside a string:
//! descriptions legitimately contain `file://{resources}`).
//!
//! `(...)` helper/key-type arguments and `{...}` metadata/attribute blocks are captured as raw
//! text spans (quote-aware, brace-balanced) because their contents (paths, nested `=`, arrays)
//! are not worth a grammar — the database only stores them opaquely.
//!
//! Every loop checks that it advanced the cursor; a stalled loop is an internal bug, reported as
//! an `Err` rather than spinning (the caller turns it into a fatal `FgdError::Parse`).

/// A lexical token with the line it started on.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// `@Name` (name without the `@`).
    Directive(String),
    /// A bare token: `[A-Za-z0-9_.-]+` (identifiers, integer/float literals, `local.origin`).
    Word(String),
    /// A quoted string, inner text verbatim (backslash escapes kept as written).
    Str(String),
    /// A `(...)` span, inner text verbatim (no nesting in real FGDs).
    Paren(String),
    /// A `{...}` span, inner text verbatim (nested braces/brackets balanced, quote-aware).
    Brace(String),
    Equal,
    Colon,
    Plus,
    LBracket,
    RBracket,
}

/// Lex `src`. `Err((line, reason))` on an unterminated string or brace span.
pub fn lex(src: &str) -> Result<Vec<Token>, (usize, String)> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    while i < chars.len() {
        let before = i;
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                let comment_before = i;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                if i == comment_before {
                    return Err((line, "internal loop guard: comment scan stalled".into()));
                }
            }
            '"' => {
                let start_line = line;
                i += 1;
                let mut s = String::new();
                loop {
                    let inner_before = i;
                    if i >= chars.len() {
                        return Err((start_line, "unterminated string".into()));
                    }
                    let c = chars[i];
                    if c == '\\' && i + 1 < chars.len() {
                        s.push(c);
                        s.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        i += 1;
                        break;
                    }
                    if c == '\n' {
                        line += 1;
                    }
                    s.push(c);
                    i += 1;
                    if i == inner_before {
                        return Err((line, "internal loop guard: string scan stalled".into()));
                    }
                }
                toks.push(Token { tok: Tok::Str(s), line: start_line });
            }
            '@' => {
                let start_line = line;
                let (name, ni) = scan_word(&chars, i + 1, line)?;
                toks.push(Token { tok: Tok::Directive(name), line: start_line });
                i = ni;
            }
            '[' => {
                toks.push(Token { tok: Tok::LBracket, line });
                i += 1;
            }
            ']' => {
                toks.push(Token { tok: Tok::RBracket, line });
                i += 1;
            }
            '=' => {
                toks.push(Token { tok: Tok::Equal, line });
                i += 1;
            }
            ':' => {
                toks.push(Token { tok: Tok::Colon, line });
                i += 1;
            }
            '+' => {
                toks.push(Token { tok: Tok::Plus, line });
                i += 1;
            }
            '(' => {
                let start_line = line;
                let (s, ni, nl) = scan_raw(&chars, i, line, ')')?;
                toks.push(Token { tok: Tok::Paren(s), line: start_line });
                i = ni;
                line = nl;
            }
            '{' => {
                let start_line = line;
                let (s, ni, nl) = scan_raw(&chars, i, line, '}')?;
                toks.push(Token { tok: Tok::Brace(s), line: start_line });
                i = ni;
                line = nl;
            }
            ',' => i += 1,
            _ => {
                let start_line = line;
                let (word, ni) = scan_word(&chars, i, line)?;
                if word.is_empty() {
                    // Unknown punctuation: skip it (never seen in real FGDs).
                    i += 1;
                } else {
                    toks.push(Token { tok: Tok::Word(word), line: start_line });
                    i = ni;
                }
            }
        }
        if i == before {
            return Err((line, "internal loop guard: lexer made no progress".into()));
        }
    }
    Ok(toks)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Scan a `[A-Za-z0-9_.-]+` word from `start`; returns the text and the index after it. The loop
/// is guarded: a scan that does not advance is an internal error, never a hang.
fn scan_word(chars: &[char], start: usize, line: usize) -> Result<(String, usize), (usize, String)> {
    let mut i = start;
    let mut s = String::new();
    while i < chars.len() && is_word_char(chars[i]) {
        let before = i;
        s.push(chars[i]);
        i += 1;
        if i == before {
            return Err((line, "internal loop guard: word scan stalled".into()));
        }
    }
    Ok((s, i))
}

/// Raw-scan a `(...)` or `{...}` span starting at the open char. Quote-aware; nested braces
/// balance (bracket arrays inside braces never contain unbalanced braces), parens do not nest.
/// Returns the inner text, the index after the closing char, and the line there.
fn scan_raw(
    chars: &[char],
    open: usize,
    mut line: usize,
    close: char,
) -> Result<(String, usize, usize), (usize, String)> {
    let start_line = line;
    let mut i = open + 1;
    let mut depth = 1usize;
    let mut s = String::new();
    while i < chars.len() {
        let before = i;
        let c = chars[i];
        if c == '"' {
            // Skip a quoted string verbatim (escapes included).
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    j += 2;
                    continue;
                }
                if chars[j] == '"' {
                    break;
                }
                j += 1;
            }
            if j >= chars.len() {
                return Err((start_line, "unterminated string in parenthesized/braced span".into()));
            }
            s.extend(&chars[i..=j]);
            i = j + 1;
            if i == before {
                return Err((line, "internal loop guard: span string scan stalled".into()));
            }
            continue;
        }
        if c == close && (close != '}' || depth == 1) {
            return Ok((s, i + 1, line));
        }
        if c == '\n' {
            line += 1;
        } else if close == '}' && c == '{' {
            depth += 1;
        } else if close == '}' && c == '}' {
            depth -= 1;
        }
        s.push(c);
        i += 1;
        if i == before {
            return Err((line, "internal loop guard: span scan stalled".into()));
        }
    }
    Err((
        start_line,
        format!("unterminated '{}' span", if close == '}' { "braced" } else { "parenthesized" }),
    ))
}
