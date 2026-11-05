//! KV1 text reader and writer — `gameinfo.gi`, `*.vdf`, `*.res`, `*.txt` scripts. A port of
//! upstream `KV1TokenReader`, `KV1TextReader`, `KVConditionEvaluator`, the KV1 object builders
//! and `KV1TextSerializer`.
//!
//! ```
//! // The shape of a game's `gameinfo.gi`: keys unquoted, tabs aligned.
//! let text = "\"GameInfo\"\n{\n\tgame\t\"Counter-Strike 2\"\n\tFileSystem\n\t{\n\t\tSearchPaths\n\t\t{\n\t\t\tGame\tcsgo\n\t\t\tGame\tcore\n\t\t}\n\t}\n}\n";
//! let doc = cs2_kv1::text::parse(text).unwrap();
//!
//! let root = doc.value.as_object().unwrap();
//! let file_system = root.get("FileSystem").unwrap().as_object().unwrap();
//! let search_paths = file_system.get("SearchPaths").unwrap().as_object().unwrap();
//! let games: Vec<_> = search_paths.get_all("Game").map(|v| v.as_str().unwrap()).collect();
//! assert_eq!(games, ["csgo", "core"]);
//!
//! // What round-trips is the document, not the bytes: the game's own file aligns its tabs and
//! // leaves keys unquoted, and the writer normalises all of that away.
//! assert_eq!(cs2_kv1::text::parse(&cs2_kv1::text::write(&doc)).unwrap(), doc);
//! ```
//!
//! # Grammar
//!
//! * A document is one **root pair**: `"GameInfo" { … }`, or a single pair `"a" "1"`. A second
//!   root object, or anything after the root has closed, is an error — upstream rejects
//!   multi-root documents the same way.
//! * Pairs are `key value` or `key = value`; keys and values are quoted or bare (a bare run
//!   ends at whitespace, `"`, `{`, `}`, `=`). `=` is only an assignment between a key and its
//!   value; elsewhere it is a one-character string.
//! * `{ … }` nests an object; `//` **and** `/` start a comment that ends at the line break.
//! * `[$WIN32]`, `[!$X360 && $PC]`, `[(1) && !(0)]` — conditionals: `$name`, digits, `!`, `&&`,
//!   `||`, parentheses. `&&` and `||` share one precedence level, left-associative, like
//!   Valve's own evaluator. A conditional may sit before the value, after `=` or after the
//!   value; the last one decides, and only one may appear in each position (two in a row is an
//!   error).
//! * `#base "file"` merges a file into this one (keys already present win, nested objects merge
//!   recursively) and `#include "file"` appends its keys, both only **outside** the root
//!   object. Both need [`ReadOptions::include_loader`]; inside an object they are ordinary
//!   pairs (key `#include`).
//! * Comments, and the exact spelling of a conditional (padding, `!`, brackets), are not
//!   normalised anywhere: a pair's condition is kept verbatim and comments are dropped.
//!
//! # Reading
//!
//! [`parse`] is [`parse_with`] with [`ReadOptions::default`]: **every** pair is kept, including
//! the ones under a false conditional, and each pair's `[$COND]` text is recorded in
//! [`KeyValue::condition`]. That is the lossless mode, and it is what a tool that edits a
//! Valve file wants — dropping a `[$WIN32]` pair because the tool runs on Linux would silently
//! rewrite the file for Windows players.
//!
//! Setting [`ReadOptions::conditions`] to a list of defined symbols (`$`-less: `WIN32`) selects
//! upstream's behaviour instead: conditions are evaluated, pairs whose condition is false are
//! dropped, a matching condition after `=` replaces an earlier pair with the same key, and no
//! pair carries a condition.
//!
//! Scalars stay [`Value::String`] with their source text unchanged (see the difference list
//! below); only `{ … }` produces [`Value::Object`].
//!
//! # Writing
//!
//! [`write`] reproduces upstream's `KV1TextSerializer` byte for byte: tabs for indentation, LF
//! line endings, every key and value quoted, a tab between a key and its value, and a newline
//! after the closing `}`. A pair that carries a [`KeyValue::condition`] writes it back — after
//! the value for a scalar pair, after the key for an object pair — so `parse(write(parse(x)))`
//! is `parse(x)` for every file that parses, conditions included (a condition is written
//! verbatim between its brackets, so a hand-built one containing `]` or a newline reads back as
//! something else). [`WriteOptions::escapes`] turns on `\\` escaping of backslashes (`"` is always
//! escaped, upstream escapes it unconditionally).
//!
//! [`write`] cannot fail: every [`Value`] can be spelled as text, and the spelling of the
//! binary-only variants is documented under `# Differences from upstream`.
//!
//! # Known lossy cases
//!
//! The "`parse(write(parse(x)))` is `parse(x)`" claim above holds for the default
//! [`ReadOptions`] only. Three things break it or a lossless read cannot carry:
//!
//! * **A matching conditional after `=` replaces an earlier pair upstream.** `xpos = 1 xpos =
//!   [$WIN32] 2` reads (with `WIN32` defined) as one pair, `xpos=2`. Lossless mode keeps only the
//!   condition *text* per pair, so it cannot say where the condition sat: evaluating the tree
//!   later keeps both pairs. Use [`ReadOptions::conditions`] when a tool has to reproduce
//!   upstream's evaluation exactly.
//! * **`#base`/`#include` merge before anything is evaluated.** The merge compares keys, not
//!   conditions, so a `[$WIN32]` pair in the including file wins over an unconditional pair of
//!   the same key from its base — and if that condition later turns out false, the base's value
//!   is already gone. Upstream merges the trees it built too; with conditions evaluated during
//!   the read, the rejected pair never takes part in the merge. (Writing such a document inlines
//!   the included items: the data model has no place for the directive.)
//! * **[`ReadOptions::valve_null_byte_bug`] keeps a raw NUL in an unquoted token.** `r { k
//!   a<NUL>b }` reads as the value `a<NUL>b` (the bug-compatible cut applies to quoted strings
//!   only), but the writer quotes it and a quoted read cuts at the NUL, so the second read is
//!   `a`.
//!
//! # Errors
//!
//! [`Error::Syntax`] carries 1-based `line`/`column` of the offending token and the message
//! text upstream produces with the position clause removed. [`Error::Include`] names an
//! included file that could not be read or parsed (its message carries the inner position as
//! `line L, column C: …`), plus a `#base`/`#include` whose loader is missing, whose path is
//! empty, or whose chain is deeper than [`MAX_INCLUDE_DEPTH`] or longer than [`MAX_INCLUDES`].
//! Messages are upstream's words, so ported test expectations stay recognisable; where upstream
//! appends the escape-sequence hint once a stray `\"` was read with [`ReadOptions::escapes`] off,
//! this port does too.
//!
//! # Limits
//!
//! * [`MAX_DEPTH`] caps object nesting at 256 (checked while reading, so a 10 000-level file is
//!   an error, not a stack overflow).
//! * [`MAX_INCLUDE_DEPTH`] caps the depth of a `#base`/`#include` chain at 64 and
//!   [`MAX_INCLUDES`] the number of files one parse may load in total (a fan-out of two per
//!   level is two to the power of the depth otherwise).
//! * A `#base` merge is linear per item (upstream's own `FindIndex` scan), so merging two large
//!   files is quadratic in their key counts; [`MAX_INCLUDES`] bounds what a hostile loader can
//!   hand back, not the size of one file.
//! * The reader never panics on hostile input: an unterminated string/condition/object, a
//!   second root object, an `#include` without a loader, a malformed conditional and a
//!   truncated file are all ordinary errors, and nothing is allocated from a length taken out
//!   of the text. The whole input is held once as a `Vec<char>` (positions are character-based,
//!   like upstream's `TextReader`), so a document costs a few bytes per character on top of the
//!   `&str` the caller already has.
//! * [`write`] recurses once per nesting level, like `cs2-kv3`'s writer; a document from
//!   [`parse`] or [`crate::binary::parse`] is capped at [`MAX_DEPTH`] levels and stays safe,
//!   and a hand-built document deeper than that is not (its `Drop` is recursive too).
//!
//! # Differences from upstream
//!
//! Each submodule's docs carry the details; together they are:
//!
//! * **Conditions are data in the default mode.** Upstream always evaluates them against
//!   `KVSerializerOptions.Conditions` and drops the pairs it rejects, never keeping the
//!   condition text. Here the default keeps every pair and records the condition
//!   ([`KeyValue::condition`]); [`ReadOptions::conditions`] = `Some(…)` switches to upstream's
//!   evaluate-and-drop behaviour.
//! * **Scalars stay text.** Upstream's `KV1TextReader.ParseValue` type-guesses (`"1"` →
//!   `Int32`, `"1.5"` → `FloatingPoint`, `0x1122334455667788` → `UInt64`, with Valve's
//!   strtol-compatible rules); this port keeps the source text in [`Value::String`], which is
//!   what makes `write` give the input back unchanged. The upstream tests that pin the guessing
//!   (`NumbersTestCase`, `TypeGuessingTestCase`, `Uint64HexTestCase`) are ported as assertions on
//!   that raw text, which is the strongest form they can take without .NET's number formatting.
//! * **The writer accepts every [`Value`]** (see the writer's docs): a colour and a blob follow
//!   upstream's text spelling, a `float` uses Rust's shortest round-trip form, and the
//!   binary-only variants that upstream cannot spell in text at all are written as their text.
//! * **Errors are values.** `KeyValueException` becomes [`Error::Syntax`] (or
//!   [`Error::Include`]), with the position in the fields instead of inside the message, and
//!   the evaluator's reason in the message instead of in an inner exception. A `#base`/
//!   `#include` whose target or owner has no root object is [`Error::Include`] where upstream
//!   throws `InvalidOperationException` from `Stack.Peek`.
//! * **Hostile input is bounded**, not fatal: [`MAX_DEPTH`] and [`MAX_INCLUDE_DEPTH`] replace
//!   upstream's unbounded recursion, and the reader is iterative. Upstream has no cap on
//!   either.
//! * **A file with more than one root object is rejected**, like upstream — including Valve's
//!   own `installscript.vdf`, which holds a signed object *and* a `"kvsignatures"` one.
//! * **Comments are dropped** (both here and upstream: upstream's KV1 builder has no place for
//!   them either), so a file with comments does not round-trip byte for byte. Neither does a
//!   file with a BOM — the reader skips one, the writer never emits one — or one whose
//!   whitespace, quote style or object layout differs from what the writer produces.
//! * **Reading and writing have their own options.** Upstream carries both directions in one
//!   `KVSerializerOptions`; here [`ReadOptions`] and [`WriteOptions`] are separate, so a
//!   round trip with escape sequences on sets [`ReadOptions::escapes`] *and*
//!   [`WriteOptions::escapes`]. Nothing is inherited silently between the two.
//! * The public API is a `&str` in, a [`Document`] out: no `Stream`, no `TextReader` and no
//!   `serialize`/`deserialize` generic on a visitor, which is why the upstream tests that go
//!   through those (reflection, `IKVTextReader`, source maps) have no counterpart here.

mod condition;
mod reader;
mod tokenizer;
mod writer;

use crate::{Document, Error};

/// Maximum number of objects that may nest, the root object included.
///
/// A deeper document is rejected with [`Error::Syntax`] when the object opens, instead of
/// growing the two reader stacks — and the returned [`Document`], whose `Drop` is recursive —
/// without bound. Upstream has no cap and recurses once per nesting level.
pub const MAX_DEPTH: usize = 256;

/// Maximum depth of a `#base`/`#include` chain.
///
/// Each included file gets its own reader (upstream builds one `KV1TextReader` per file), so
/// the chain is recursion here; a deeper or self-including chain is [`Error::Include`].
/// Upstream recurses with no cap.
pub const MAX_INCLUDE_DEPTH: usize = 64;

/// Maximum number of files one [`parse`] may load through `#base`/`#include`.
///
/// [`MAX_INCLUDE_DEPTH`] bounds how *deep* a chain goes, not how wide it fans out: a file that
/// names two others, each of which does the same, is 2ⁿ documents at depth n, and upstream keeps
/// one reader and one builder per file until the whole tree is finalized, so it runs out of
/// memory. The per-parse cap makes that an [`Error::Include`] instead. A real `gameinfo.gi`
/// chain is under ten files.
pub const MAX_INCLUDES: usize = 1024;

/// A `#base`/`#include` resolver: the path as written in the file to that file's text
/// (upstream `IIncludedFileLoader`). An `Err` is reported as [`Error::Include`] with the path.
pub type IncludeLoader<'a> = dyn Fn(&str) -> Result<String, String> + 'a;

/// Options for [`parse_with`]. Upstream `KVSerializerOptions`.
#[derive(Clone, Default)]
pub struct ReadOptions<'a> {
    /// Defined `$`-less symbols (`WIN32`, `X360`) to evaluate `[$COND]` against, like upstream
    /// `KVSerializerOptions.Conditions`.
    ///
    /// `None` (the default) is this port's lossless mode: nothing is evaluated away, every pair
    /// is kept and its condition is recorded in [`KeyValue::condition`]. `Some(list)` evaluates
    /// conditions like upstream — pairs whose condition is false are dropped, a matching
    /// condition after `=` replaces an earlier pair with the same key, and `condition` stays
    /// `None` on every pair. An empty list is *not* `None`: it evaluates every conditional
    /// against no symbols, so all of them are false — upstream's own default list is never
    /// empty (`WIN32` plus `LINUX`/`POSIX`, `WINDOWS`, or `OSX`/`POSIX` for the platform it runs
    /// on), so `Some(vec!["WIN32".into(), "LINUX".into(), "POSIX".into()])` is what upstream
    /// reads a Linux `gameinfo.gi` with.
    pub conditions: Option<Vec<String>>,
    /// Translate escape sequences (`\n`, `\t`, `\"`, …) inside quoted strings — upstream
    /// `HasEscapeSequences`. Off by default, exactly like upstream: with it off a backslash is
    /// literal, and a `\"` in a file is a broken quote that the errors point out.
    pub escapes: bool,
    /// Valve bug-for-bug compatibility with tier1 `KeyValues`/`CUtlBuffer` (upstream
    /// `EnableValveNullByteBugBehavior`): an unknown escape sequence reads as a NUL byte and
    /// the string is cut at it, instead of being an error. Off by default.
    pub valve_null_byte_bug: bool,
    /// Resolver for `#base` and `#include` (see [`IncludeLoader`]). `None` makes an inclusion
    /// an [`Error::Include`], like upstream without a `FileLoader`.
    pub include_loader: Option<&'a IncludeLoader<'a>>,
}

/// Options for [`write_with`]. Upstream `KVSerializerOptions.HasEscapeSequences`.
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// Escape backslashes as `\\`. Off by default (upstream writes `\` for `\` with escape
    /// sequences off); `"` is escaped either way.
    pub escapes: bool,
}

/// Prints the options; the include loader is a closure, so only its presence is shown.
impl std::fmt::Debug for ReadOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadOptions")
            .field("conditions", &self.conditions)
            .field("escapes", &self.escapes)
            .field("valve_null_byte_bug", &self.valve_null_byte_bug)
            .field("include_loader", &self.include_loader.map(|_| "<loader>"))
            .finish()
    }
}

/// Parses a KV1 text document with [`ReadOptions::default`].
pub fn parse(text: &str) -> Result<Document, Error> {
    parse_with(text, &ReadOptions::default())
}

/// Parses a KV1 text document.
///
/// The returned [`Document`] is the root pair; duplicate keys keep their order and are all
/// present, and each pair's condition is in [`KeyValue::condition`] unless
/// [`ReadOptions::conditions`] asked for upstream's evaluate-and-drop behaviour.
pub fn parse_with(text: &str, opts: &ReadOptions<'_>) -> Result<Document, Error> {
    reader::parse(text, opts)
}

/// Writes a document as KV1 text with [`WriteOptions::default`].
pub fn write(doc: &Document) -> String {
    write_with(doc, &WriteOptions::default())
}

/// Writes a document as KV1 text, byte for byte the way upstream's `KV1TextSerializer` does
/// (plus the conditions of pairs that carry one).
///
/// Infallible: every [`Value`] has a text spelling, so there is nothing to fail with.
pub fn write_with(doc: &Document, opts: &WriteOptions) -> String {
    writer::write(doc, opts)
}
