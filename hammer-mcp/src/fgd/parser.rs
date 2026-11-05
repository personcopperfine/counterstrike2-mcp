//! Parser for a lexed FGD file: directives -> class declarations (`RawClass`), excludes and
//! `@include`s. Anything a class cannot be parsed from is reported as a warning with its line and
//! skipped up to the next `@` directive — never silently dropped, never fatal (fatal errors —
//! unterminated strings, include cycles, a missing root file — are raised by the caller).
//!
//! Every loop carries a progress guard: if an iteration does not move the cursor the parser records
//! a fatal parse error and stops, so a parser bug can never hang or allocate without bound.

use super::lexer::{lex, Tok, Token};
use super::{ClassKind, Choice, Flag, KeyDef};
use std::path::Path;

/// Header keywords that appear without a `(…)` argument list in the real game files
/// (`@PointClass halfgridsnap editormodel(...) base(...)`); anything else bare is a typo.
const BARE_HELPERS: [&str; 1] = ["halfgridsnap"];

/// One class declaration as written (own keys only, bases unresolved).
#[derive(Debug, Clone, PartialEq)]
pub struct RawClass {
    pub kind: ClassKind,
    /// `@OverrideClass` merges into the class of the same name instead of defining it.
    pub is_override: bool,
    pub name: String,
    pub description: Option<String>,
    pub bases: Vec<String>,
    pub keys: Vec<KeyDef>,
    /// name, type, description — `declared_in` is filled in during assembly.
    pub inputs: Vec<(String, String, Option<String>)>,
    pub outputs: Vec<(String, String, Option<String>)>,
    /// Header helpers (`editormodel`, `iconsprite`, `size`, `tags`, `metadata`, ...) as
    /// name -> raw args text.
    pub helpers: Vec<(String, String)>,
    pub line: usize,
}

/// One top-level item in file order — the order matters: an `@exclude` hides only the class
/// definitions that precede it.
#[derive(Debug)]
pub enum ParsedItem {
    /// `@exclude Name` (the name lowercased).
    Exclude(String),
    Class(RawClass),
}

/// One parsed file: everything the database assembly needs.
#[derive(Debug, Default)]
pub struct ParsedFile {
    /// `@include "x"` entries as (name, line) — resolved by the caller.
    pub includes: Vec<(String, usize)>,
    /// `@exclude`s and class declarations in file order.
    pub items: Vec<ParsedItem>,
    /// Parse problems (line, text) — surfaced as `Database::warnings`.
    pub warnings: Vec<(usize, String)>,
}

/// Lex and parse one FGD file.
pub fn parse_file(_path: &Path, text: &str) -> Result<ParsedFile, (usize, String)> {
    let toks = lex(text)?;
    let mut p = Parser {
        toks,
        pos: 0,
        out: ParsedFile::default(),
        fatal: None,
    };
    p.top_level();
    match p.fatal {
        Some(err) => Err(err),
        None => Ok(p.out),
    }
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    out: ParsedFile,
    /// A loop that did not advance is a parser bug: recorded here and returned as a fatal
    /// `Err` by `parse_file` (never an infinite loop).
    fatal: Option<(usize, String)>,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn peek_tok(&self) -> Option<Tok> {
        self.toks.get(self.pos).map(|t| t.tok.clone())
    }

    fn line(&self) -> usize {
        self.peek().map(|t| t.line).unwrap_or(0)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn warn(&mut self, line: usize, text: impl Into<String>) {
        self.out.warnings.push((line, text.into()));
    }

    /// Record a fatal parse error (the first one wins).
    fn fail(&mut self, line: usize, reason: impl Into<String>) {
        if self.fatal.is_none() {
            self.fatal = Some((line, reason.into()));
        }
    }

    /// Loop guard: a loop body must move its cursor (`before` -> `now`) every iteration.
    /// Returns true — after recording a fatal parse error — when it did not.
    fn stuck(&mut self, before: usize, now: usize, line: usize, what: &str) -> bool {
        if before == now {
            self.fail(
                line,
                format!("internal loop guard: {what} made no progress at token {before}"),
            );
            return true;
        }
        false
    }

    /// A quoted string with `"a " + "b"` continuations joined into one text.
    fn text(&mut self) -> Option<String> {
        let first = match self.peek_tok() {
            Some(Tok::Str(s)) => {
                self.pos += 1;
                s
            }
            _ => return None,
        };
        let mut text = first;
        loop {
            let before = self.pos;
            match self.peek_tok() {
                Some(Tok::Plus) => match self.toks.get(self.pos + 1).map(|t| t.tok.clone()) {
                    Some(Tok::Str(s)) => {
                        text.push_str(&s);
                        self.pos += 2;
                    }
                    _ => break,
                },
                _ => break,
            }
            if self.stuck(before, self.pos, self.line(), "text") {
                break;
            }
        }
        Some(text)
    }

    fn top_level(&mut self) {
        while self.fatal.is_none() {
            let before = self.pos;
            let line = self.line();
            let Some(t) = self.next() else { break };
            match t.tok {
                Tok::Directive(name) => {
                    let lower = name.to_ascii_lowercase();
                    if lower == "include" {
                        match self.next() {
                            Some(Token {
                                tok: Tok::Str(s),
                                line,
                            }) => {
                                self.out.includes.push((s, line));
                            }
                            Some(t) => self.warn(t.line, "@include without a quoted file name"),
                            None => self.warn(line, "@include without a quoted file name"),
                        }
                    } else if lower == "exclude" {
                        match self.next().map(|t| t.tok) {
                            Some(Tok::Word(w)) | Some(Tok::Str(w)) => {
                                self.out.items.push(ParsedItem::Exclude(w.to_ascii_lowercase()))
                            }
                            _ => self.warn(line, "@exclude without a class name"),
                        }
                    } else if lower.ends_with("class") {
                        match self.class_decl(&lower, t.line) {
                            Ok(raw) => self.out.items.push(ParsedItem::Class(raw)),
                            Err((line, reason)) => {
                                self.warn(line, reason);
                                self.skip_to_directive();
                            }
                        }
                    } else {
                        // `@EntityGroup "…" { … }`, `@VisGroupFilter { … }`, `@mapsize(…)`,
                        // `@MaterialExclusion [ … ]`, `@AutoVisGroup …` — not entity data; skip.
                        self.skip_unknown_directive();
                    }
                }
                other => {
                    self.warn(t.line, format!("unexpected {other:?} outside a directive"));
                }
            }
            if self.stuck(before, self.pos, line, "top_level") {
                break;
            }
        }
    }

    /// Skip tokens up to (not including) the next `@` directive — the recovery for a class that
    /// could not be parsed.
    fn skip_to_directive(&mut self) {
        loop {
            let before = self.pos;
            let line = self.line();
            match self.peek() {
                Some(t) if matches!(t.tok, Tok::Directive(_)) => return,
                Some(_) => self.pos += 1,
                None => return,
            }
            if self.stuck(before, self.pos, line, "skip_to_directive") {
                return;
            }
        }
    }

    /// Consume the value-ish tokens that follow an unknown directive (strings, words, raw
    /// `(...)`/`{...}` spans and balanced `[ ... ]` groups), stopping at the next directive.
    fn skip_unknown_directive(&mut self) {
        loop {
            let before = self.pos;
            let line = self.line();
            match self.peek_tok() {
                Some(Tok::Str(_) | Tok::Word(_) | Tok::Paren(_) | Tok::Brace(_)) => self.pos += 1,
                Some(Tok::LBracket) => self.skip_bracket_group(),
                _ => return,
            }
            if self.stuck(before, self.pos, line, "skip_unknown_directive") {
                return;
            }
        }
    }

    /// Skip a balanced `[ ... ]` group.
    fn skip_bracket_group(&mut self) {
        let mut depth = 0usize;
        loop {
            let before = self.pos;
            let line = self.line();
            match self.next() {
                Some(Token {
                    tok: Tok::LBracket, ..
                }) => depth += 1,
                Some(Token {
                    tok: Tok::RBracket, ..
                }) => depth = depth.saturating_sub(1),
                Some(_) => {}
                None => return,
            }
            if depth == 0 {
                return;
            }
            if self.stuck(before, self.pos, line, "skip_bracket_group") {
                return;
            }
        }
    }

    /// Parse a `@PointClass`/`@SolidClass`/`@BaseClass`/`@*Class` declaration.
    fn class_decl(&mut self, directive: &str, line: usize) -> Result<RawClass, (usize, String)> {
        let kind = match directive {
            // Every `@*Class` that Hammer places as a point entity, not just `@PointClass`:
            // filters, NPCs, path nodes, keyframe/move ropes and cables are all point entities.
            "pointclass" | "npcclass" | "filterclass" | "pathclass" | "pathnodeclass"
            | "keyframeclass" | "moveclass" | "cableclass" => ClassKind::Point,
            "solidclass" => ClassKind::Solid,
            "baseclass" => ClassKind::Base,
            _ => ClassKind::Other,
        };
        let is_override = directive == "overrideclass";
        let mut bases = Vec::new();
        let mut helpers: Vec<(String, String)> = Vec::new();

        // Header: helper tokens until `=`.
        loop {
            let before = self.pos;
            match self.peek_tok() {
                Some(Tok::Word(word)) => {
                    let word_line = self.line();
                    self.pos += 1;
                    // `sphere()` (an empty argument list) is common in the real files and is a
                    // normal helper; only a word with no argument list at all is suspicious.
                    let args = match self.peek_tok() {
                        Some(Tok::Paren(p)) | Some(Tok::Brace(p)) => {
                            self.pos += 1;
                            Some(p)
                        }
                        _ => None,
                    };
                    if word.eq_ignore_ascii_case("base") {
                        for b in args.unwrap_or_default().split(',') {
                            let b = b.trim();
                            if !b.is_empty() {
                                bases.push(b.to_string());
                            }
                        }
                    } else if args.is_none()
                        && !BARE_HELPERS.contains(&word.to_ascii_lowercase().as_str())
                    {
                        // Real headers parenthesize their helpers; a bare word means the header
                        // is not parseable (`halfgridsnap` is the one bare keyword in the game
                        // files). Report it at the word's own line and let the caller recover.
                        return Err((
                            word_line,
                            format!("@{directive}: helper '{word}' has no '(…)' argument list"),
                        ));
                    } else {
                        helpers.push((word, args.unwrap_or_default()));
                    }
                }
                Some(Tok::Equal) => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err((
                        self.line(),
                        format!("@{directive}: expected a helper or '=', found {other:?}"),
                    ));
                }
            }
            if self.stuck(before, self.pos, self.line(), "class_decl header") {
                return Err(self
                    .fatal
                    .clone()
                    .unwrap_or((line, "internal loop guard".to_string())));
            }
        }

        // Class name.
        let name = match self.peek_tok() {
            Some(Tok::Word(w)) => {
                self.pos += 1;
                w
            }
            other => {
                return Err((
                    line,
                    format!("@{directive}: missing class name after '=' (found {other:?})"),
                ))
            }
        };

        // Optional `: "description"` (absent when `:` is directly followed by the body `[`).
        let mut description = None;
        if matches!(self.peek_tok(), Some(Tok::Colon)) {
            self.pos += 1;
            if !matches!(self.peek_tok(), Some(Tok::LBracket)) {
                description = self.text();
            }
        }

        // Body `[ ... ]` (an empty `[]` is common).
        let mut keys = Vec::new();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        if matches!(self.peek_tok(), Some(Tok::LBracket)) {
            let body = self.body_tokens();
            self.parse_body(&body, &name, &mut keys, &mut inputs, &mut outputs);
        }

        Ok(RawClass {
            kind,
            is_override,
            name,
            description,
            bases,
            keys,
            inputs,
            outputs,
            helpers,
            line,
        })
    }

    /// Consume the `[` and collect the tokens up to the matching `]` as a sub-slice (an
    /// unterminated body runs to the end of the file — the caller sees the truncated slice).
    fn body_tokens(&mut self) -> Vec<Token> {
        self.pos += 1; // the `[`
        let start = self.pos;
        let mut depth = 1usize;
        loop {
            let before = self.pos;
            let line = self.line();
            let Some(t) = self.peek() else { break };
            match t.tok {
                Tok::LBracket => depth += 1,
                Tok::RBracket => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                break;
            }
            self.pos += 1;
            if self.stuck(before, self.pos, line, "body_tokens") {
                break;
            }
        }
        let end = self.pos.min(self.toks.len());
        if matches!(self.peek().map(|t| t.tok.clone()), Some(Tok::RBracket)) {
            self.pos += 1;
        }
        self.toks[start..end].to_vec()
    }

    fn parse_body(
        &mut self,
        body: &[Token],
        class_name: &str,
        keys: &mut Vec<KeyDef>,
        inputs: &mut Vec<(String, String, Option<String>)>,
        outputs: &mut Vec<(String, String, Option<String>)>,
    ) {
        let mut warns_sink: Vec<(usize, String)> = Vec::new();
        let mut i = 0usize;
        while i < body.len() {
            let before = i;
            let start_line = body[i].line;
            let name = match &body[i].tok {
                Tok::Word(w) => w.clone(),
                other => {
                    warns_sink.push((
                        start_line,
                        format!("class '{class_name}': unexpected {other:?} in body"),
                    ));
                    i += 1;
                    if self.stuck_at(before, i, class_name, "body key") {
                        break;
                    }
                    continue;
                }
            };
            i += 1;
            if name.eq_ignore_ascii_case("input") || name.eq_ignore_ascii_case("output") {
                // input Name(type) [attrs] {attrs} [: "description"]
                let (io_name, io_type) = match io_head(body, &mut i) {
                    Some(v) => v,
                    None => {
                        warns_sink.push((
                            start_line,
                            format!("class '{class_name}': malformed {name} line"),
                        ));
                        if self.stuck_at(before, i, class_name, "input/output line") {
                            break;
                        }
                        continue;
                    }
                };
                skip_attr_groups(body, &mut i);
                let mut desc = None;
                if matches!(body.get(i).map(|t| &t.tok), Some(Tok::Colon)) {
                    i += 1;
                    desc = text_at(body, &mut i);
                }
                if name.eq_ignore_ascii_case("input") {
                    inputs.push((io_name, io_type, desc));
                } else {
                    outputs.push((io_name, io_type, desc));
                }
                if self.stuck_at(before, i, class_name, "input/output line") {
                    break;
                }
                continue;
            }
            // Key: Name(type) [attrs] {attrs} [: display [: default [: desc]]] [= [values]]
            let type_ = match body.get(i).map(|t| t.tok.clone()) {
                Some(Tok::Paren(p)) => {
                    i += 1;
                    p.trim().to_ascii_lowercase()
                }
                _ => {
                    warns_sink.push((
                        start_line,
                        format!("class '{class_name}': key '{name}' without a (type)"),
                    ));
                    if self.stuck_at(before, i, class_name, "key type") {
                        break;
                    }
                    continue;
                }
            };
            skip_attr_groups(body, &mut i);
            // Value segments: `: display [: default [: desc]]`; an empty segment (`: : "desc"`)
            // is `None` in its position.
            let mut segs: Vec<Option<String>> = Vec::new();
            while matches!(body.get(i).map(|t| &t.tok), Some(Tok::Colon)) {
                let seg_before = i;
                i += 1;
                match body.get(i).map(|t| &t.tok) {
                    Some(Tok::Str(_)) => segs.push(text_at(body, &mut i)),
                    // A bare word is a value (`: 0`, `: -1`, `: 1.5`) unless it is the next key's
                    // name — real files end a key with a dangling `: ` right before the next key
                    // (core/lights2.fgd), and that segment is empty.
                    Some(Tok::Word(_)) if word_is_value(body, i) => segs.push(text_at(body, &mut i)),
                    Some(Tok::Colon) => segs.push(None), // empty segment (`: : "desc"`)
                    _ => {
                        // Trailing `:` with the value missing (`… : "desc" :` then `]`, or the
                        // next key/input line).
                        segs.push(None);
                        break;
                    }
                }
                if self.stuck_at(seg_before, i, class_name, "value segment") {
                    break;
                }
            }
            let display_name = segs.first().cloned().flatten();
            let default = segs.get(1).cloned().flatten();
            let description = segs.get(2).cloned().flatten();

            // Choices / flags block.
            let mut choices = Vec::new();
            let mut flags = Vec::new();
            if matches!(body.get(i).map(|t| &t.tok), Some(Tok::Equal)) {
                i += 1;
                if !matches!(body.get(i).map(|t| &t.tok), Some(Tok::LBracket)) {
                    warns_sink.push((
                        body.get(i).map(|t| t.line).unwrap_or(start_line),
                        format!("class '{class_name}': key '{name}': '=' without a [ … ] value block"),
                    ));
                } else {
                    i += 1;
                    let mut depth = 1usize;
                    while i < body.len() && depth > 0 {
                        let value_before = i;
                        let vline = body[i].line;
                        match body[i].tok.clone() {
                            Tok::Str(value) | Tok::Word(value) => {
                                i += 1;
                                choice_entry(
                                    body,
                                    &mut i,
                                    &type_,
                                    value,
                                    vline,
                                    &mut choices,
                                    &mut flags,
                                    class_name,
                                    &name,
                                    &mut warns_sink,
                                );
                            }
                            Tok::LBracket => {
                                depth += 1;
                                i += 1;
                            }
                            Tok::RBracket => {
                                depth -= 1;
                                i += 1;
                            }
                            other => {
                                warns_sink.push((
                                    vline,
                                    format!(
                                        "class '{class_name}': key '{name}': unexpected {other:?} in value list"
                                    ),
                                ));
                                i += 1;
                            }
                        }
                        if self.stuck_at(value_before, i, class_name, "value list") {
                            break;
                        }
                    }
                }
            }
            keys.push(KeyDef {
                name,
                type_,
                display_name,
                default,
                description,
                choices,
                flags,
                declared_in: class_name.to_string(),
            });
            if self.stuck_at(before, i, class_name, "body key") {
                break;
            }
        }
        self.out.warnings.extend(warns_sink);
    }

    /// `stuck` for the index-based loops over a body slice: a body that cannot advance is a
    /// parser bug, not a file quirk.
    fn stuck_at(&mut self, before: usize, now: usize, class_name: &str, what: &str) -> bool {
        if before == now {
            self.fail(
                0,
                format!(
                    "internal loop guard: {what} of class '{class_name}' made no progress at token {before}"
                ),
            );
            return true;
        }
        false
    }
}

/// One `value : "label" [: default_on]` entry inside a choices/flags block; advances `i` past
/// what it consumed.
#[allow(clippy::too_many_arguments)]
fn choice_entry(
    body: &[Token],
    i: &mut usize,
    type_: &str,
    value: String,
    vline: usize,
    choices: &mut Vec<Choice>,
    flags: &mut Vec<Flag>,
    class_name: &str,
    key: &str,
    warns: &mut Vec<(usize, String)>,
) {
    // `: label [: on/off]`
    let mut label = None;
    let mut default_on = false;
    if matches!(body.get(*i).map(|t| &t.tok), Some(Tok::Colon)) {
        *i += 1;
        label = match body.get(*i).map(|t| &t.tok) {
            Some(Tok::Str(s)) => Some(s.clone()),
            Some(Tok::Word(w)) => Some(w.clone()),
            _ => None,
        };
        if label.is_some() {
            *i += 1;
        }
        if matches!(body.get(*i).map(|t| &t.tok), Some(Tok::Colon)) {
            *i += 1;
            match body.get(*i).map(|t| t.tok.clone()) {
                Some(Tok::Word(w)) | Some(Tok::Str(w)) => {
                    default_on = w == "1";
                    *i += 1;
                }
                _ => {}
            }
        }
    }
    // Further `: "…"` segments are the choice's description (`core/markup_volumes.fgd`,
    // `core/lights2.fgd`); `Choice`/`Flag` do not carry one, so they are consumed and dropped.
    while matches!(body.get(*i).map(|t| &t.tok), Some(Tok::Colon)) {
        let before = *i;
        *i += 1;
        match body.get(*i).map(|t| &t.tok) {
            Some(Tok::Str(_) | Tok::Word(_)) => {
                let _extra = text_at(body, i);
            }
            _ => break,
        }
        if *i == before {
            break;
        }
    }
    if type_ == "flags" {
        match value.parse::<i64>() {
            Ok(bit) => flags.push(Flag {
                bit: bit as u64,
                label: label.unwrap_or_default(),
                default_on,
            }),
            Err(_) => warns.push((
                vline,
                format!("class '{class_name}': key '{key}': flag bit '{value}' is not an integer"),
            )),
        }
    } else {
        choices.push(Choice {
            value,
            label: label.unwrap_or_default(),
        });
    }
}

/// True when the bare word at `i` is a `: value` segment rather than the next body entry's name.
/// Real files write a dangling `: ` before the next key or `input`/`output` line, whose name is
/// followed by its `(type)`/`{…}`.
fn word_is_value(body: &[Token], i: usize) -> bool {
    let starts_an_entry = match body.get(i).map(|t| &t.tok) {
        Some(Tok::Word(w)) => {
            w.eq_ignore_ascii_case("input")
                || w.eq_ignore_ascii_case("output")
                || matches!(
                    body.get(i + 1).map(|t| &t.tok),
                    Some(Tok::Paren(_) | Tok::Brace(_))
                )
        }
        _ => false,
    };
    !starts_an_entry
}

/// `Name(type)` of an input/output line; returns (name, type) and advances past both.
fn io_head(body: &[Token], i: &mut usize) -> Option<(String, String)> {
    let name = match body.get(*i).map(|t| t.tok.clone()) {
        Some(Tok::Word(w)) => {
            *i += 1;
            w
        }
        _ => return None,
    };
    match body.get(*i).map(|t| t.tok.clone()) {
        Some(Tok::Paren(p)) => {
            *i += 1;
            Some((name, p.trim().to_ascii_lowercase()))
        }
        _ => None,
    }
}

/// Skip `[ … ]` attribute groups and raw `{ … }` spans (in any order, any number).
fn skip_attr_groups(body: &[Token], i: &mut usize) {
    loop {
        let before = *i;
        match body.get(*i).map(|t| &t.tok) {
            Some(Tok::Brace(_)) => *i += 1,
            Some(Tok::LBracket) => {
                let mut depth = 0usize;
                while *i < body.len() {
                    let inner_before = *i;
                    match body[*i].tok {
                        Tok::LBracket => depth += 1,
                        Tok::RBracket => depth -= 1,
                        _ => {}
                    }
                    *i += 1;
                    if depth == 0 {
                        break;
                    }
                    if *i == inner_before {
                        break; // progress guard; the caller's guard ends the class
                    }
                }
            }
            _ => return,
        }
        // The `[ … ]` arm always advances; the guard only ever fires on an internal bug, in
        // which case stopping is better than spinning (the caller's own guard then ends the
        // class with a fatal parse error).
        if *i == before {
            return;
        }
    }
}

/// Read a text value (string with `+` continuations, or a bare word) at position `i` of `body`.
fn text_at(body: &[Token], i: &mut usize) -> Option<String> {
    let first = match body.get(*i).map(|t| &t.tok) {
        Some(Tok::Str(s)) => {
            *i += 1;
            s.clone()
        }
        Some(Tok::Word(w)) => {
            *i += 1;
            return Some(w.clone());
        }
        _ => return None,
    };
    let mut text = first;
    loop {
        let before = *i;
        match body.get(*i).map(|t| &t.tok) {
            Some(Tok::Plus) => match body.get(*i + 1).map(|t| &t.tok) {
                Some(Tok::Str(s)) => {
                    text.push_str(s);
                    *i += 2;
                }
                _ => break,
            },
            _ => break,
        }
        if *i == before {
            break;
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop guard itself: a body that does not move the cursor records a fatal `Parse`
    /// error (which `parse_file` turns into `Err`), instead of spinning.
    #[test]
    fn loop_guard_records_a_fatal_parse_error_when_the_position_does_not_move() {
        let mut p = Parser {
            toks: Vec::new(),
            pos: 7,
            out: ParsedFile::default(),
            fatal: None,
        };
        assert!(p.stuck(7, 7, 12, "unit"), "a stalled loop is fatal");
        assert_eq!(p.fatal.as_ref().map(|(l, _)| *l), Some(12));
        assert!(p.stuck_at(3, 3, "some_class", "unit"));
        assert!(!p.stuck(7, 8, 12, "unit"), "progress is not an error");
        assert!(!p.stuck_at(3, 4, "some_class", "unit"));
    }
}
