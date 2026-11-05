//! Port of upstream `KV1TextReader`, `KV1TextReaderStateMachine`, `KV1TextReaderState` and the
//! KV1 use of `KVObjectBuilder` / `KVMergingObjectBuilder` / `KVAppendingObjectBuilder`.
//!
//! Two stacks drive the parse, exactly as upstream: a **state machine** of object frames that
//! decides which token may appear where (and carries the per-object "a condition was false"
//! flag), and a **builder** that turns the accepted tokens into the returned tree. Upstream
//! splits the builder into a main class plus two subclasses; here one `Builder` carries a
//! `BuilderKind` and the owner index of the `#base`/`#include` document that created it.
//!
//! The reader is iterative: hostile nesting grows the two stacks (both capped, see
//! [`super::MAX_DEPTH`]) instead of the call stack, and a `#base`/`#include` chain recurses one
//! Rust frame per file (capped, see [`super::MAX_INCLUDE_DEPTH`]).
//!
//! # Differences from upstream
//!
//! * **Conditions are data.** With [`ReadOptions::conditions`] = `None` (the default) nothing
//!   is dropped: every pair is kept and its `[$COND]` text goes into
//!   [`crate::KeyValue::condition`]. Upstream always evaluates conditions against
//!   `KVSerializerOptions.Conditions` and drops the pairs whose condition is false, keeping no
//!   record of them. A pair with several conditionals keeps the last one ("the last one
//!   decides", upstream's own rule for *keeping* a pair) — upstream additionally acts on an
//!   earlier one: a matching conditional after `=` replaces the pair with the same key that came
//!   before it, which is recorded here as nothing but the text (see the crate's
//!   `# Known lossy cases`).
//! * **Values stay text.** A scalar is [`Value::String`] with the source text unchanged.
//!   Upstream type-guesses with `KV1TextReader.ParseValue`: `"1"` becomes an `Int32`, `"1.5"`
//!   a `FloatingPoint`, a `0x` + 16 hex digits a `UInt64`. That guessing is what upstream's
//!   `NumbersTestCase`, `TypeGuessingTestCase` and `Uint64HexTestCase` pin, including .NET's
//!   round-trip float formatting on the way out; keeping the text is lossless (`write` gives
//!   the input back) and leaves the interpretation to the caller.
//! * **Errors are [`Error::Syntax`] with the position in the fields.** Upstream throws
//!   `KeyValueException` with `at line …` inside the message; the messages here are the
//!   upstream texts with that clause removed and, where a clause followed the position, joined
//!   with `; `. An invalid conditional's message also carries the evaluator's reason, which
//!   upstream keeps in the exception's `InnerException`.
//! * **Nesting is capped** ([`super::MAX_DEPTH`]) like the include chain
//!   ([`super::MAX_INCLUDE_DEPTH`], and [`super::MAX_INCLUDES`] for how many files one parse may
//!   load); upstream
//!   recurses and a deep enough file overflows its stack. The cap is checked when an object
//!   opens, so an over-deep document is an ordinary error.
//! * **A document or included file with no root object is an error with a position**
//!   (`Builder is not in a fully completed state (stack count is 0).` at the end of that file).
//!   Upstream reaches the same rejection through `KVObjectBuilder.GetObject`, which throws a
//!   `KeyValueException` with the count but no position, and only after the document's
//!   inclusions have been loaded: a `#base x` with nothing else reports the missing loader (or
//!   the failing include) first, here as well. When it is an *included* file the error is an
//!   [`Error::Include`] naming that file, with the position in the message; every other error
//!   from an included file is wrapped the same way, so a caller can tell which file of a chain
//!   failed (upstream's exceptions carry no path at all). A `#base`/`#include` whose *target*
//!   has no root object is therefore an include error, and one whose *including document* has
//!   none (and whose inclusions all loaded) is a syntax error at the end of that document.
//! * **The missing-loader message names this API.** Upstream's text is `Inclusions require a
//!   FileLoader to be provided in KVSerializerOptions.`; the [`Error::Include`] here says
//!   `inclusions require ReadOptions::include_loader to be set`, because that is the option a
//!   caller of this crate has to set.
//! * **A document has exactly one root object**, so a file with two is rejected where it
//!   starts (`Found data after the root object; documents with multiple root objects are not
//!   supported.`). Valve's own `installscript.vdf` files are exactly that — a signed object
//!   plus a `"kvsignatures"` one — so they do not parse, here or upstream; the
//!   `text_real_files` test lists them by name.
//! * **Key comparisons are ASCII-case-insensitive**, where upstream's `RemoveItem` uses the
//!   runtime's `OrdinalIgnoreCase` (so `É` and `é` are the same key upstream and different keys
//!   here); the merge and lookup paths compare ordinally, exactly like upstream's. The two
//!   document-level directives (`#base`, `#include`) are compared the same way, where upstream
//!   uses `OrdinalIgnoreCase` too.
//! * **A `}` as the very first token ends the document pseudo-object** and the reader then runs
//!   out of frames, so it reports `Found end of file when another token type was expected.` at
//!   the end of the input. Upstream pops the pseudo-object, stops reading and reports
//!   `Builder is not in a fully completed state (stack count is 0).` for the same input; both
//!   reject it.
//! * **Positions count characters, upstream counts UTF-16 code units**, so a column after an
//!   astral character on the same line differs by one. Input is a `&str`, so a UTF-16 file (a
//!   Source 1 localisation file, which upstream's `StreamReader` decodes because of its BOM) has
//!   to be decoded by the caller.
//! * **Hostile input cannot allocate unboundedly.** `#include` chains, object nesting and the
//!   size of a single token are all bounded by the input length, and a token grows the string
//!   it is reading one character at a time (upstream's `StringBuilder` does the same).
//! * `DiscardCurrentObject` with an empty builder stack is a no-op instead of upstream's
//!   `Stack.Peek` throw; that state is unreachable from text input (it needs a false condition
//!   in a position where the reader has already rejected the input).

use super::condition::evaluate_condition;
use super::tokenizer::{Token, TokenKind, Tokenizer};
use super::{ReadOptions, MAX_DEPTH, MAX_INCLUDE_DEPTH, MAX_INCLUDES};
use crate::{Document, Error, KeyValue, Object, Value};

/// Upstream `KV1TextReaderState`. The two document-level include states are the reader's way of
/// waiting for an inclusion's path.
///
/// The variants keep upstream's `InObject…`/`InDocument…` spelling because the error messages
/// quote them (`Found conditional while in state InObjectBeforeKey.`), which is what makes a
/// ported test expectation recognisable.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    InObjectBeforeKey,
    InObjectBetweenKeyAndValue,
    InObjectAfterAssignment,
    InObjectAfterValue,
    InDocumentBeforeIncludePath,
    InDocumentBeforeBasePath,
}

impl State {
    /// The name upstream's error messages use (the C# enum member).
    fn name(self) -> &'static str {
        match self {
            State::InObjectBeforeKey => "InObjectBeforeKey",
            State::InObjectBetweenKeyAndValue => "InObjectBetweenKeyAndValue",
            State::InObjectAfterAssignment => "InObjectAfterAssignment",
            State::InObjectAfterValue => "InObjectAfterValue",
            State::InDocumentBeforeIncludePath => "InDocumentBeforeIncludePath",
            State::InDocumentBeforeBasePath => "InDocumentBeforeBasePath",
        }
    }
}

/// One object frame of the state machine (upstream `KVPartialState<KV1TextReaderState>`).
struct Frame {
    /// The states this object's position can be in, innermost last.
    states: Vec<State>,
    /// The key of the pair being read inside this frame (upstream `KVPartialState.Key`).
    name: Option<String>,
    /// Whether a conditional that evaluated to false was seen in this object: upstream's
    /// `SetDiscardCurrent` marks the object, and finalizing it then discards one item.
    discard: bool,
    /// Lossless mode: the condition of the pair this frame is reading.
    pending_condition: Option<String>,
}

/// Upstream `KV1TextReaderStateMachine`.
struct StateMachine {
    frames: Vec<Frame>,
}

impl StateMachine {
    /// Upstream's constructor: a document-level pseudo-object already in `BeforeKey`.
    fn new() -> Self {
        Self {
            frames: vec![Frame {
                states: vec![State::InObjectBeforeKey],
                name: None,
                discard: false,
                pending_condition: None,
            }],
        }
    }

    fn is_in_object(&self) -> bool {
        !self.frames.is_empty()
    }

    /// Upstream `IsAtDocumentLevel`: only the document pseudo-object is open.
    fn is_at_document_level(&self) -> bool {
        self.frames.len() == 1
    }

    /// The current state (upstream `Current` = `CurrentObject.States.Peek()`). Every frame gets
    /// a state before the reader can look at it; the fallback keeps a hostile input from
    /// panicking where upstream's `Stack.Peek` would throw.
    fn current(&self) -> State {
        self.frames
            .last()
            .and_then(|frame| frame.states.last())
            .copied()
            .unwrap_or(State::InObjectBeforeKey)
    }

    /// Upstream `PushObject`: opens a frame for an object's contents, with no state in it yet.
    fn push_object(&mut self) {
        self.frames.push(Frame {
            states: Vec::new(),
            name: None,
            discard: false,
            pending_condition: None,
        });
    }

    fn push(&mut self, state: State) {
        if let Some(frame) = self.frames.last_mut() {
            frame.states.push(state);
        }
    }

    /// Upstream `Pop`: drops the innermost state of the current frame (an inclusion's path was
    /// consumed).
    fn pop(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.states.pop();
        }
    }

    /// Upstream `PopObject`: closes the current frame, returning its discard flag.
    fn pop_object(&mut self) -> bool {
        self.frames.pop().map(|frame| frame.discard).unwrap_or(false)
    }

    fn current_name(&self) -> Option<String> {
        self.frames.last().and_then(|frame| frame.name.clone())
    }

    fn set_name(&mut self, name: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.name = Some(name);
        }
    }

    /// Upstream `SetDiscardCurrent`.
    fn set_discard_current(&mut self, discard: bool) {
        if let Some(frame) = self.frames.last_mut() {
            frame.discard = discard;
        }
    }

    fn take_pending_condition(&mut self) -> Option<String> {
        self.frames.last_mut().and_then(|frame| frame.pending_condition.take())
    }

    fn set_pending_condition(&mut self, condition: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pending_condition = Some(condition);
        }
    }
}

/// One partial object state of a builder (upstream `KVPartialState`; the array and flag bits
/// are KV3/array machinery the KV1 text reader never touches).
struct Partial {
    /// The key this state's pair gets (upstream `KVPartialState.Key`).
    key: Option<String>,
    /// Set for a scalar: the state *is* a pair with that value and `items` is unused (upstream
    /// `KVPartialState.Value`, which `MakeObject` returns as-is).
    value: Option<Value>,
    items: Vec<KeyValue>,
    /// The pair's `[$COND]` text in lossless mode; set at `{` for an object pair and at the
    /// value for a scalar one.
    condition: Option<String>,
}

impl Partial {
    fn new(key: Option<String>, condition: Option<String>) -> Self {
        Self { key, value: None, items: Vec::new(), condition }
    }

    /// Upstream `MakeObject`: the scalar if there is one, else a collection of `items`.
    fn into_pair(self) -> KeyValue {
        let value = match self.value {
            Some(value) => value,
            None => Value::Object(Object { entries: self.items }),
        };
        KeyValue { key: self.key.unwrap_or_default(), value, condition: self.condition }
    }
}

/// Which of upstream's three builders this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuilderKind {
    /// The document itself (`KVObjectBuilder`).
    Main,
    /// A `#base` document (`KVMergingObjectBuilder`): keys already in the owner win, new keys
    /// are appended, nested collections merge recursively.
    Merge,
    /// An `#include` document (`KVAppendingObjectBuilder`): all items land after the owner's.
    Append,
}

/// Upstream `KVObjectBuilder` and its two subclasses, flattened onto one type.
struct Builder {
    kind: BuilderKind,
    /// The builder whose top state receives the finished items (upstream `originalBuilder`).
    /// Meaningless for [`BuilderKind::Main`].
    owner: usize,
    /// The `#base`/`#include` builders this builder's document created, in creation order
    /// (upstream `associatedBuilders`).
    associates: Vec<usize>,
    /// The open objects, innermost last (upstream `stateStack`).
    stack: Vec<Partial>,
}

impl Builder {
    fn main() -> Self {
        Self { kind: BuilderKind::Main, owner: 0, associates: Vec::new(), stack: Vec::new() }
    }

    fn include(kind: BuilderKind, owner: usize) -> Self {
        Self { kind, owner, associates: Vec::new(), stack: Vec::new() }
    }

    /// Upstream `OnKeyValuePair`: a scalar pair goes into the current object, or *becomes* the
    /// document when the document is a single pair (`"a" "1"`).
    fn key_value_pair(&mut self, key: String, value: Value, condition: Option<String>) {
        match self.stack.last_mut() {
            Some(state) => state.items.push(KeyValue { key, value, condition }),
            None => self
                .stack
                .push(Partial { key: Some(key), value: Some(value), items: Vec::new(), condition }),
        }
    }

    /// Upstream `OnObjectStart`: opens the state the object's items go into.
    fn begin_object(&mut self, key: Option<String>, condition: Option<String>) {
        self.stack.push(Partial::new(key, condition));
    }

    /// Upstream `OnObjectEnd`: closes the innermost state and appends it to its parent. The
    /// document's own root object is the exception (`if (Count <= 1) return;` upstream): it
    /// stays on the stack for [`Builder::take`], because the document *is* that pair.
    fn end_object(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }

        let Some(state) = self.stack.pop() else {
            return;
        };
        if let Some(parent) = self.stack.last_mut() {
            parent.items.push(state.into_pair());
        }
    }

    /// Upstream `DiscardCurrentObject`: removes the item that was just added, or the whole
    /// state when it has no items (the object whose condition was false was already opened).
    fn discard_current_object(&mut self) {
        match self.stack.last_mut() {
            Some(state) if !state.items.is_empty() => {
                state.items.pop();
            }
            Some(_) => {
                self.stack.pop();
            }
            // Unreachable from text input (see the module docs); upstream's `Stack.Peek` would
            // throw here.
            None => {}
        }
    }

    /// Upstream `RemoveItem`: drops the first item with this key, used when a conditional that
    /// *matches* follows an assignment and replaces the earlier pair. The comparison is
    /// case-insensitive (`StringComparison.OrdinalIgnoreCase` upstream — the one key comparison
    /// in the readers that is; the merge and lookup paths are ordinal).
    fn remove_item(&mut self, name: &str) {
        if let Some(state) = self.stack.last_mut() {
            if let Some(index) = state.items.iter().position(|kv| kv.key.eq_ignore_ascii_case(name)) {
                let _ = state.items.remove(index);
            }
        }
    }

    /// Lossless mode: the condition of the pair that just ended (upstream's
    /// `SetDiscardCurrent` position `InObjectAfterValue`).
    fn set_last_condition(&mut self, condition: String) {
        if let Some(state) = self.stack.last_mut() {
            if let Some(last) = state.items.last_mut() {
                last.condition = Some(condition);
            }
        }
    }

    /// Upstream `GetObject`: the finished document, or `None` when the builder is not in the
    /// one-state shape a document needs.
    fn take(&mut self) -> Option<Document> {
        if self.stack.len() != 1 {
            return None;
        }
        self.stack.pop().map(Partial::into_pair)
    }
}

/// One text document being read: the tokenizer, the state machine and the include paths seen
/// so far. A `#base`/`#include` target gets its own `Parser` (upstream builds a fresh
/// `KV1TextReader` per included file) that shares the builders with its includer.
struct Parser<'a> {
    opts: &'a ReadOptions<'a>,
    builders: &'a mut Vec<Builder>,
    tok: Tokenizer,
    sm: StateMachine,
    /// Whether the previous token was a conditional (only one may appear in each position).
    read_condition: bool,
    /// Paths seen under `#base`, in order (upstream `ItemsForMerging`).
    merging: Vec<String>,
    /// Paths seen under `#include`, in order (upstream `ItemsForAppending`).
    appending: Vec<String>,
    /// Position of this document's end-of-input token, for the error [`Parser::take_root`] can
    /// still report.
    eof_line: usize,
    eof_column: usize,
}

/// Parses one text document (upstream `KV1TextReader.ReadHeader` plus the listener calls).
pub(super) fn parse(text: &str, opts: &ReadOptions<'_>) -> Result<Document, Error> {
    let mut builders = vec![Builder::main()];
    let mut parser = Parser::new(opts, &mut builders, text);
    parser.run(0, 0)?;
    parser.take_root(0)
}

impl<'a> Parser<'a> {
    fn new(opts: &'a ReadOptions<'a>, builders: &'a mut Vec<Builder>, text: &str) -> Self {
        Self {
            opts,
            builders,
            tok: Tokenizer::new(text, opts.escapes, opts.valve_null_byte_bug),
            sm: StateMachine::new(),
            read_condition: false,
            merging: Vec::new(),
            appending: Vec::new(),
            eof_line: 1,
            eof_column: 1,
        }
    }

    /// Whether conditions are kept instead of evaluated ([`ReadOptions::conditions`] = `None`).
    fn lossless(&self) -> bool {
        self.opts.conditions.is_none()
    }

    /// A syntax error at `token`'s position, with the escape-sequence hint upstream appends
    /// once a stray `\"` was read with escape sequences disabled.
    fn syntax(&self, token: &Token, message: impl Into<String>) -> Error {
        Error::Syntax {
            line: token.line,
            column: token.column,
            message: format!("{}{}", message.into(), self.tok.escape_hint()),
        }
    }

    /// Upstream `Found data after the root object …`.
    fn data_after_root(&self, token: &Token) -> Error {
        self.syntax(
            token,
            "Found data after the root object; documents with multiple root objects are not supported.",
        )
    }

    /// The condition of the pair being read, in lossless mode only: with a condition list the
    /// pairs carry no condition (upstream has no place to put one).
    fn take_pending_condition(&mut self) -> Option<String> {
        if self.lossless() {
            self.sm.take_pending_condition()
        } else {
            None
        }
    }

    /// The main token loop (upstream `ReadHeader`).
    fn run(&mut self, builder: usize, include_depth: usize) -> Result<(), Error> {
        loop {
            let token = self.tok.read_next_token()?;
            let kind = classify(&self.sm, &token);

            match kind {
                TokenKind::Key | TokenKind::String => self.read_text(builder, &token)?,
                TokenKind::Assignment => self.read_assignment(&token)?,
                TokenKind::ObjectStart => self.begin_new_object(builder, &token)?,
                TokenKind::ObjectEnd => {
                    // A '}' at document level after the root has closed would otherwise pop the
                    // document pseudo-object and silently ignore the rest of the input.
                    if self.sm.is_at_document_level()
                        && self.sm.current() == State::InObjectAfterValue
                    {
                        return Err(self.data_after_root(&token));
                    }

                    self.finalize_current_object(builder, true, &token)?;
                }
                TokenKind::Condition => self.handle_condition(builder, &token)?,
                // Comments are skipped; their text is not part of the data model.
                TokenKind::Comment => continue,
                TokenKind::IncludeAndAppend => {
                    self.sm.push(State::InDocumentBeforeIncludePath);
                }
                TokenKind::IncludeAndMerge => {
                    self.sm.push(State::InDocumentBeforeBasePath);
                }
                TokenKind::EndOfFile => {
                    self.eof_line = token.line;
                    self.eof_column = token.column;
                    self.finalize_document(builder, &token, include_depth)?;
                    return Ok(());
                }
            }

            self.read_condition = kind == TokenKind::Condition;
        }
    }

    /// Upstream `ReadText`: a string or key token, interpreted from the current state.
    fn read_text(&mut self, builder: usize, token: &Token) -> Result<(), Error> {
        match self.sm.current() {
            // A value already ended: this text starts the next pair in the enclosing object.
            State::InObjectAfterValue => {
                if self.sm.is_at_document_level() {
                    return Err(self.data_after_root(token));
                }

                self.finalize_current_object(builder, false, token)?;
                self.sm.push_object();
                self.set_object_key(&token.value);
                Ok(())
            }
            State::InObjectBeforeKey => {
                self.set_object_key(&token.value);
                Ok(())
            }
            State::InObjectBetweenKeyAndValue | State::InObjectAfterAssignment => {
                let condition = self.take_pending_condition();
                let name = self.sm.current_name().unwrap_or_default();
                self.builders[builder].key_value_pair(
                    name,
                    Value::String(token.value.clone()),
                    condition,
                );
                self.sm.push(State::InObjectAfterValue);
                Ok(())
            }
            State::InDocumentBeforeIncludePath | State::InDocumentBeforeBasePath => {
                self.add_inclusion(&token.value, token)
            }
        }
    }

    fn set_object_key(&mut self, name: &str) {
        self.sm.set_name(name.to_string());
        self.sm.push(State::InObjectBetweenKeyAndValue);
    }

    /// Upstream `ReadAssignment`: `=` only means assignment between a key and its value;
    /// anywhere else [`classify`] has already turned it into a string.
    fn read_assignment(&mut self, token: &Token) -> Result<(), Error> {
        if self.sm.is_at_document_level() {
            return Err(self.syntax(
                token,
                "Found '=' after the root key; the root key must be followed by '{'.",
            ));
        }

        self.sm.push(State::InObjectAfterAssignment);
        Ok(())
    }

    /// Upstream `BeginNewObject`.
    fn begin_new_object(&mut self, builder: usize, token: &Token) -> Result<(), Error> {
        let current = self.sm.current();
        if !matches!(current, State::InObjectBetweenKeyAndValue | State::InObjectAfterAssignment)
        {
            return Err(self.syntax(
                token,
                format!("Attempted to begin new object while in state {}.", current.name()),
            ));
        }

        // Upstream recurses per object; the cap keeps a hostile document from building a tree
        // that is unsound to hold (its `Drop` is recursive too).
        if self.builders[builder].stack.len() >= MAX_DEPTH {
            return Err(self.syntax(
                token,
                format!("Object nesting is deeper than the supported maximum of {MAX_DEPTH}."),
            ));
        }

        let name = self.sm.current_name();
        let condition = self.take_pending_condition();
        self.builders[builder].begin_object(name, condition);
        self.sm.push_object();
        self.sm.push(State::InObjectBeforeKey);
        Ok(())
    }

    /// Upstream `FinalizeCurrentObject`.
    fn finalize_current_object(
        &mut self,
        builder: usize,
        explicit: bool,
        token: &Token,
    ) -> Result<(), Error> {
        let current = self.sm.current();
        if !matches!(current, State::InObjectBeforeKey | State::InObjectAfterValue) {
            return Err(self.syntax(
                token,
                format!("Attempted to finalize object while in state {}.", current.name()),
            ));
        }

        let discard = self.sm.pop_object();
        if self.sm.is_in_object() {
            self.sm.push(State::InObjectAfterValue);
        }

        if discard && !self.lossless() {
            self.builders[builder].discard_current_object();
        }

        if explicit {
            self.builders[builder].end_object();
        }

        Ok(())
    }

    /// Upstream `FinalizeDocument`: closes the root object, then reads every `#base` and then
    /// every `#include` of this document.
    fn finalize_document(
        &mut self,
        builder: usize,
        token: &Token,
        include_depth: usize,
    ) -> Result<(), Error> {
        if !self.sm.is_at_document_level()
            || !matches!(self.sm.current(), State::InObjectBeforeKey | State::InObjectAfterValue)
        {
            return Err(self.syntax(token, "Found end of file when another token type was expected."));
        }

        self.finalize_current_object(builder, true, token)?;

        for path in std::mem::take(&mut self.merging) {
            self.include(&path, BuilderKind::Merge, builder, include_depth)?;
        }
        for path in std::mem::take(&mut self.appending) {
            self.include(&path, BuilderKind::Append, builder, include_depth)?;
        }

        // Upstream reaches this rejection in `KVObjectBuilder.GetObject`, with the count but no
        // position, and only after every inclusion has been loaded (so a missing loader or a
        // broken include is reported before it); an empty document, or one that is nothing but
        // inclusions, lands here.
        let count = self.builders[builder].stack.len();
        if count != 1 {
            return Err(self.syntax(
                token,
                format!("Builder is not in a fully completed state (stack count is {count})."),
            ));
        }

        Ok(())
    }

    /// Upstream `ReadText`'s inclusion states: record the path and leave the include state.
    fn add_inclusion(&mut self, path: &str, token: &Token) -> Result<(), Error> {
        if path.is_empty() {
            return Err(self.syntax(token, "Found an inclusion directive with an empty file path."));
        }

        if self.sm.current() == State::InDocumentBeforeBasePath {
            self.merging.push(path.to_string());
        } else {
            self.appending.push(path.to_string());
        }

        self.sm.pop();
        Ok(())
    }

    /// Upstream `DoIncludeAndMerge` / `DoIncludeAndAppend`: reads the included file into its
    /// own builder with the same options.
    fn include(
        &mut self,
        path: &str,
        kind: BuilderKind,
        owner: usize,
        include_depth: usize,
    ) -> Result<(), Error> {
        if include_depth >= MAX_INCLUDE_DEPTH {
            return Err(Error::Include {
                path: path.to_string(),
                message: format!(
                    "#base/#include nesting is deeper than the supported maximum of {MAX_INCLUDE_DEPTH}."
                ),
            });
        }

        // One builder per included file is kept until the whole tree is finalized, so the number
        // of files a single parse can load is capped as well as the depth (a two-way fan-out
        // would otherwise be 2^depth documents).
        if self.builders.len() > MAX_INCLUDES {
            return Err(Error::Include {
                path: path.to_string(),
                message: format!(
                    "the document includes more than {MAX_INCLUDES} files in total."
                ),
            });
        }

        let Some(loader) = self.opts.include_loader else {
            // Upstream: `Inclusions require a FileLoader to be provided in KVSerializerOptions.`
            return Err(Error::Include {
                path: path.to_string(),
                message: "inclusions require ReadOptions::include_loader to be set".to_string(),
            });
        };

        let text = match loader(path) {
            Ok(text) => text,
            Err(message) => return Err(Error::Include { path: path.to_string(), message }),
        };

        let index = self.builders.len();
        self.builders.push(Builder::include(kind, owner));
        self.builders[owner].associates.push(index);

        let mut included = Parser::new(self.opts, self.builders, &text);
        included.run(index, include_depth + 1).map_err(|err| match err {
            // A syntax error inside the included file is reported as an include error naming that
            // file, because the position alone would read as if it were the including document's
            // (upstream's exceptions carry no path either). An include error already names a file.
            Error::Syntax { line, column, message } => Error::Include {
                path: path.to_string(),
                message: format!("line {line}, column {column}: {message}"),
            },
            other => other,
        })
    }

    /// Upstream `HandleCondition`.
    fn handle_condition(&mut self, builder: usize, token: &Token) -> Result<(), Error> {
        let current = self.sm.current();
        if !matches!(
            current,
            State::InObjectAfterValue
                | State::InObjectBetweenKeyAndValue
                | State::InObjectAfterAssignment
        ) {
            return Err(self.syntax(
                token,
                format!("Found conditional while in state {}.", current.name()),
            ));
        }

        // A conditional between a root object's name and its '{' is valid and applies to that
        // object, but after the root has closed it would discard the whole document.
        if current == State::InObjectAfterValue && self.sm.is_at_document_level() {
            return Err(self.data_after_root(token));
        }

        if self.read_condition {
            return Err(self.syntax(token, "Found a second consecutive conditional."));
        }

        let text = token.value.as_str();
        let matches = match self.opts.conditions.as_deref() {
            // Lossless mode has no defined symbols: the expression is still validated (and a
            // malformed one is still an error) but its text is what gets recorded.
            None => {
                self.evaluate_condition(&[], text, token)?;
                false
            }
            Some(defined) => self.evaluate_condition(defined, text, token)?,
        };

        if self.lossless() {
            match current {
                // The pair already ended: its condition is recorded on it.
                State::InObjectAfterValue => {
                    self.builders[builder].set_last_condition(text.to_string());
                }
                // The pair is still being read: the condition is carried to it.
                _ => self.sm.set_pending_condition(text.to_string()),
            }
        } else {
            // A pair can have a conditional before '=', after '=', and after its value. The
            // last one decides.
            self.sm.set_discard_current(!matches);

            // A matching conditional after '=' replaces the earlier pair with the same key.
            if matches && current == State::InObjectAfterAssignment {
                let name = self.sm.current_name().unwrap_or_default();
                self.builders[builder].remove_item(&name);
            }
        }

        Ok(())
    }

    /// Upstream `KVConditionEvaluator` use: an invalid expression is a syntax error at the
    /// conditional's own position, carrying the evaluator's reason.
    fn evaluate_condition(
        &self,
        defined: &[String],
        text: &str,
        token: &Token,
    ) -> Result<bool, Error> {
        evaluate_condition(defined, text)
            .map_err(|reason| self.syntax(token, format!("Invalid conditional syntax \"{text}\" ({})", reason.0)))
    }

    /// Upstream `KVObjectBuilder.GetObject`: finalizes every `#base`/`#include` this document (and
    /// its includes) created, then returns the root pair.
    fn take_root(&mut self, main: usize) -> Result<Document, Error> {
        finalize_all(self.builders, main)?;

        let count = self.builders[main].stack.len();
        self.builders[main].take().ok_or_else(|| Error::Syntax {
            line: self.eof_line,
            column: self.eof_column,
            message: format!("Builder is not in a fully completed state (stack count is {count})."),
        })
    }
}

/// Upstream `ClassifyToken`: the token reader cannot tell how a string or `=` is used, so the
/// reader decides from the state — `=` is an assignment only between a key and its value, keys
/// appear at the start of an object or right after the previous value, and outside the root
/// object `#include` and `#base` are directives.
fn classify(sm: &StateMachine, token: &Token) -> TokenKind {
    if token.kind == TokenKind::Assignment
        && sm.current() != State::InObjectBetweenKeyAndValue
    {
        return TokenKind::String;
    }

    if token.kind == TokenKind::String
        && matches!(sm.current(), State::InObjectBeforeKey | State::InObjectAfterValue)
    {
        if sm.is_at_document_level() && token.value.eq_ignore_ascii_case("#include") {
            return TokenKind::IncludeAndAppend;
        }

        if sm.is_at_document_level() && token.value.eq_ignore_ascii_case("#base") {
            return TokenKind::IncludeAndMerge;
        }

        return TokenKind::Key;
    }

    token.kind
}

/// Upstream `KVObjectBuilder.GetObject`'s associate walk plus the two subclass finalizers:
/// every builder this one created is finalized first, then its items are merged into (or
/// appended to) its owner's top state. Index 0 is the main document.
fn finalize_all(builders: &mut Vec<Builder>, index: usize) -> Result<(), Error> {
    for associate in std::mem::take(&mut builders[index].associates) {
        finalize_all(builders, associate)?;
    }

    let kind = builders[index].kind;
    if kind == BuilderKind::Main {
        return Ok(());
    }

    let owner = builders[index].owner;

    // Both stacks normally hold exactly one state here: `finalize_document` checks a document's
    // own count once its inclusions are loaded, and a failed check aborts the parse before this
    // walk runs (`None` is the document-without-a-root-object case, which never gets here).
    //
    // Two disjoint mutable borrows: this document's top state and its owner's. `owner < index`
    // always holds, because the owner existed before this builder was created.
    let (before, after) = builders.split_at_mut(index);
    let Some(from) = after[0].stack.last_mut() else {
        return Ok(());
    };
    let Some(into) = before[owner].stack.last_mut() else {
        return Ok(());
    };

    match kind {
        BuilderKind::Merge => merge_items(from, into),
        BuilderKind::Append => into.items.append(&mut from.items),
        BuilderKind::Main => {}
    }

    Ok(())
}

/// Upstream `KVMergingObjectBuilder.Merge(KVPartialState, KVPartialState)`: an item whose key
/// the owner already has merges into the owner's value, everything else is appended. The key
/// comparison is ordinal (`i.Key == item.Key`), case-sensitive.
fn merge_items(from: &mut Partial, into: &mut Partial) {
    for item in std::mem::take(&mut from.items) {
        match into.items.iter().position(|existing| existing.key == item.key) {
            Some(index) => merge_value(item.value, &mut into.items[index].value),
            None => into.items.push(item),
        }
    }
}

/// Upstream `KVMergingObjectBuilder.Merge(KVObject, KVObject)`: only collections merge, child by
/// child; a scalar on either side, or a non-collection owner, has nothing to merge.
fn merge_value(from: Value, into: &mut Value) {
    let Value::Object(from) = from else {
        return;
    };
    let Value::Object(into) = into else {
        return;
    };

    for item in from.entries {
        match into.entries.iter_mut().find(|existing| existing.key == item.key) {
            Some(matching) => merge_value(item.value, &mut matching.value),
            // The owner is a collection here, so a new key is appended. Upstream's `Add`
            // throws for a dictionary-backed owner; KV1 text always builds list-backed ones.
            None => into.entries.push(item),
        }
    }
}
