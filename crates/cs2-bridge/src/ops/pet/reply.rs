//! The pure half of the `pet.*` port: every reply and error object the group builds, as a
//! function of plain inputs.
//!
//! The C writes these into one `SB` and the wire bytes are the contract, so every builder here
//! reproduces the C's `sb_add` sequence in order, with the C's mark/rollback (`out->len = mark;
//! out->buf[mark] = 0;`) and its 512-byte scratch rules for `OpErr::data`. The functions take
//! strings, numbers and small structs a thin reader filled from game memory ([`super::game`]),
//! which is what lets `tests/ops_pet_reply.rs` hold them to the C byte for byte without a game.
//!
//! [`Buf`] is the C's `SB` (`mcp_bridge.c:228-245`) plus the two things the C's `pet.*` code uses
//! that [`Sb`] does not expose: the write cursor (`out->len` as a mark) and rollback to it.
//! `Sb`'s `len` is private and `entity_json.rs` is outside this lane's files, so the six-line
//! cursor lives here; the escaping and number formatting are still `Sb`'s (see [`Buf::addjson`]).

use crate::entity_json::{self, Sb};
use crate::kv3_check;
use crate::ops::{OpErr, OP_ERR_DATA_CAP};

// ---------------------------------------------------------------------------------------------
// The C's constants (`mcp_bridge_pet.inc`)
// ---------------------------------------------------------------------------------------------

/// `PET_VPCF_MAX` (`mcp_bridge_pet.inc:252`): 16 MB, far past any real `.vpcf` and the bound on
/// the transient allocation the load gate makes on the main thread.
pub const PET_VPCF_MAX: u64 = 16 << 20;

/// `PET_FILE_TYPE_DISK` (`mcp_bridge_pet.inc:257`): `GetFileType`'s disk type. Only a real file
/// is read - a FIFO or device named `*.vpcf` would block the main thread inside `ReadFile`.
pub const PET_FILE_TYPE_DISK: u32 = 1;

/// `PET_TEXT_MAX` (`mcp_bridge_pet.inc:801`): 384 KB of raw `.vpcf` text, request and reply.
pub const PET_TEXT_MAX: usize = 393216;

/// `PET_KV3CTX_BYTES` (`mcp_bridge_pet.inc:802`): the static KV3 parse context's size. The RE
/// says the real frame is 4608 bytes (`0x12A0`); 16 KB over-allocates.
pub const PET_KV3CTX_BYTES: usize = 16384;

/// `PET_LABEL_MAX` (`mcp_bridge_pet.inc:803`): the `pet.apply` label cap.
pub const PET_LABEL_MAX: usize = 64;

/// `PET_LOADKV3_FLAGS` (`mcp_bridge_pet.inc:804`): pet's own text-overload callers pass 0.
pub const PET_LOADKV3_FLAGS: u32 = 0;

/// `PET_LOADKV3_SYM` (`mcp_bridge_pet.inc:805`): tier0's text overload of `LoadKV3`, resolved
/// per call (never cached across module state).
pub const PET_LOADKV3_SYM: &str =
    "?LoadKV3@@YA_NPEAVKeyValues3@@PEAVCUtlString@@PEBDAEBUKV3ID_t@@2I@Z";

/// The nine function-array types the root node walks (`for int t = 0; t <= 8; t++`,
/// `mcp_bridge_pet.inc:189`).
pub const ARRAY_TYPES: usize = 9;

/// The seven operator types the registry reports (`for int t = 0; t <= 6; t++`,
/// `mcp_bridge_pet.inc:608`).
pub const FUNCTION_TYPES: usize = 7;

/// The depth the schema walks stop at (`pet_schema_walk`'s `depth > 8`, `pet_schema_bases`'
/// `depth >= 8`, `mcp_bridge_pet.inc:666`, `:683`).
pub const SCHEMA_DEPTH: i32 = 8;

// ---------------------------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------------------------

/// The C's `SB` (`mcp_bridge.c:228-245`) with a write cursor and rollback.
///
/// `putc` is the C's rule exactly: a byte that would need the terminator's slot sets `ovf` and
/// is refused (`mcp_bridge.c:230-234`), so a buffer of `cap` bytes holds at most `cap - 1`
/// characters. `ovf` is sticky and `rollback` does not clear it, like the C's `b->ovf` (which
/// `out->len = mark` leaves alone).
pub struct Buf {
    data: Vec<u8>,
    cap: usize,
    ovf: bool,
    scratch: Vec<u8>,
}

impl Buf {
    /// `sb_init` (`mcp_bridge.c:229`) over a `cap`-byte buffer.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            data: Vec::new(),
            cap,
            ovf: false,
            scratch: Vec::new(),
        }
    }

    /// The C's `b->cap`.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// The C's `b->len`: the write cursor, which is what a mark is.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The C's `b->ovf`.
    #[must_use]
    pub fn ovf(&self) -> bool {
        self.ovf
    }

    /// The bytes written so far, without the terminator (the C string `b->buf`).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// The C's `u32 mark = out->len;`.
    #[must_use]
    pub fn mark(&self) -> usize {
        self.data.len()
    }

    /// The C's `out->len = mark; out->buf[mark] = 0;`: drop everything written since `mark`.
    /// `ovf` is left alone, as the C leaves it.
    pub fn rollback(&mut self, mark: usize) {
        self.data.truncate(mark);
    }

    /// `sb_putc` (`mcp_bridge.c:230-234`).
    pub fn putc(&mut self, byte: u8) {
        if self.data.len() + 1 >= self.cap {
            self.ovf = true;
            return;
        }
        self.data.push(byte);
    }

    /// `sb_add` (`mcp_bridge.c:235`): a C string, stopping at its NUL.
    pub fn add(&mut self, text: &[u8]) {
        for &byte in cstr(text) {
            self.putc(byte);
        }
    }

    /// `sb_addnum` (`mcp_bridge.c:236-241`).
    pub fn addnum(&mut self, mut value: u64) {
        let mut digits = [0u8; 24];
        let mut count = 0;
        if value == 0 {
            digits[count] = b'0';
            count += 1;
        }
        while value != 0 {
            digits[count] = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
        }
        while count > 0 {
            count -= 1;
            self.putc(digits[count]);
        }
    }

    /// `sb_addi` (`mcp_bridge.c:242-245`).
    pub fn addi(&mut self, value: i64) {
        if value < 0 {
            self.putc(b'-');
        }
        self.addnum(value.unsigned_abs());
    }

    /// `sb_addjson` (`mcp_bridge.c:289-319`), delegated to [`Sb`] so the escape table and the
    /// U+FFFD rule are the crate's one copy.
    ///
    /// The scratch buffer is sized for the worst case: a control byte becomes `\u00XX` (six
    /// bytes) and an invalid lead byte becomes one three-byte U+FFFD, so six bytes per input
    /// byte plus the two quotes is always enough and `Sb` never overflows here.
    pub fn addjson(&mut self, text: &[u8]) {
        let need = 6 * text.len() + 3;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0);
        }
        let escaped: Vec<u8> = {
            let mut sb = Sb::new(&mut self.scratch[..need]);
            sb.addjson(text);
            debug_assert!(!sb.ovf(), "the `addjson` scratch is sized for the worst case");
            sb.as_cstr().to_vec()
        };
        self.add(&escaped);
    }

    /// A `true` / `false` literal.
    pub fn addbool(&mut self, value: bool) {
        self.add(if value { b"true" } else { b"false" });
    }
}

/// The C string `b`, i.e. `b` up to the first NUL. `Sb::add` / `sb_add` read strings that way,
/// so every input this module forwards to it is cut the same way.
#[must_use]
pub fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

/// `cpy` appended to a growable buffer: appends at most `cap - 1 - len(out)` bytes, stopping at
/// `src`'s NUL. This is the C's `cpy(msg + n, part, sizeof msg - n)` chain.
fn cpy_append(out: &mut Vec<u8>, part: &[u8], cap: usize) {
    let room = cap.saturating_sub(1 + out.len());
    let part = cstr(part);
    out.extend_from_slice(&part[..part.len().min(room)]);
}

// ---------------------------------------------------------------------------------------------
// G1/G2: the loadability refusals and the log lines beside them
// ---------------------------------------------------------------------------------------------

/// Builds a 512-byte `OpErr::data` object (`char data[512]`, `mcp_bridge.c:1988`), and says
/// whether it fit.
fn data_object(build: impl FnOnce(&mut Buf)) -> (String, bool) {
    let mut buf = Buf::new(OP_ERR_DATA_CAP);
    build(&mut buf);
    // The C hands `e->data` (a `char[]`, so NUL-terminated) to `sb_add`, which stops at the NUL:
    // `bytes()` is already that prefix.
    (String::from_utf8_lossy(buf.bytes()).into_owned(), buf.ovf())
}

/// `pet_unloadable` (`mcp_bridge_pet.inc:304-335`): the `-32602 UnloadableFile` error whose
/// `data` is `{"reason":<reason>,"path":<path>}`.
///
/// The 512-byte `data` cannot always carry a long path, and a raw cut would leave a JSON string
/// with no closing quote, so the path is shortened from the left with a `~` marking the cut
/// (`:316-334`); when not one byte is left the path becomes `null` (`:326-331`).
///
/// `path` is the game's own bytes (`GetPath` / `kv3_host_path`), never a lossily decoded string:
/// `sb_addjson` is the C's byte-wise escaper, so the reply is identical for a code-page path.
#[must_use]
pub fn unloadable(reason: &str, path: &[u8]) -> OpErr {
    let whole = |p: &[u8]| {
        // The whole path when it fits (`mcp_bridge_pet.inc:308-315`).
        data_object(|b| {
            b.add(b"{\"reason\":");
            b.addjson(reason.as_bytes());
            b.add(b",\"path\":");
            b.addjson(p);
            b.add(b"}");
        })
    };
    let (data, ovf) = whole(path);
    if !ovf {
        return OpErr {
            code: -32602,
            name: "UnloadableFile",
            data,
        };
    }
    // `pbuf[0] = '~'; cpy(pbuf + 1, path, sizeof pbuf - 1);` (`:316-317`): a 1040-byte buffer,
    // so at most 1038 bytes of path after the marker.
    let mut pbuf: Vec<u8> = Vec::new();
    pbuf.push(b'~');
    cpy_append_marker(&mut pbuf, path, 1040);
    loop {
        let (data, ovf) = whole(&pbuf);
        if !ovf {
            return OpErr {
                code: -32602,
                name: "UnloadableFile",
                data,
            };
        }
        let n = pbuf.len();
        if n <= 1 {
            // `:326-331`: nothing left to cut, keep the JSON valid.
            let (data, _) = data_object(|b| {
                b.add(b"{\"reason\":");
                b.addjson(reason.as_bytes());
                b.add(b",\"path\":null}");
            });
            return OpErr {
                code: -32602,
                name: "UnloadableFile",
                data,
            };
        }
        pbuf.drain(1..2); // `:332-333`: drop one byte after the marker
    }
}

/// `cpy(pbuf + 1, path, 1039)`: the C's buffer is 1040 bytes and the call passes `sizeof - 1`, so
/// the destination may hold 1038 characters plus the NUL.
fn cpy_append_marker(out: &mut Vec<u8>, part: &[u8], cap: usize) {
    let room = cap.saturating_sub(1 + out.len());
    let part = cstr(part);
    out.extend_from_slice(&part[..part.len().min(room)]);
}

/// `pet_log_refusal` (`mcp_bridge_pet.inc:338-353`): the one log line a refusal writes,
/// `"pet: <what> refused <reason>: <path>"`, in the C's 1200-byte buffer.
///
/// The log is not the wire, so a code-page path is decoded lossily here; the reply's `data`
/// keeps the raw bytes ([`unloadable`]).
#[must_use]
pub fn log_refusal(what: &str, reason: &str, path: &[u8]) -> String {
    let mut msg: Vec<u8> = Vec::with_capacity(1200);
    for part in [&b"pet: "[..], what.as_bytes(), b" refused ", reason.as_bytes(), b": "] {
        cpy_append(&mut msg, part, 1200);
    }
    cpy_append(&mut msg, path, 1200);
    String::from_utf8_lossy(&msg).into_owned()
}

/// A pre-load gate's refusal: the wire error and the log line the C writes before answering it.
pub struct Refused {
    /// The `-32602 UnloadableFile` reply's error.
    pub error: OpErr,
    /// The `pet: <what> refused <reason>: <path>` line.
    pub log: String,
}

impl Refused {
    /// The wire error and the log line for one refusal.
    #[must_use]
    pub fn new(what: &str, reason: &str, path: &[u8]) -> Self {
        Self {
            error: unloadable(reason, path),
            log: log_refusal(what, reason, path),
        }
    }
}

/// What the host file read answered for one gate (`pet_read_vpcf`,
/// `mcp_bridge_pet.inc:263-299`): the bytes, or the C's `why` out-parameter.
pub enum FileRead<'a> {
    /// `why` is `"too_large"` or `"read_failed"` (`:261-262`).
    Refused(&'a str),
    /// The file's bytes (`lenOut` of them).
    Bytes(&'a [u8]),
}

/// `pet_open_gate`'s decision (`mcp_bridge_pet.inc:378-414`) over the facts the process and the
/// file supply.
///
/// * `path` - the caller's addon-relative path, which the two path-shape refusals report;
/// * `app` / `addon` / `host` - `editor_app_root`, `kv3_addon_name` and `kv3_host_path`, each an
///   `Option` because each can fail;
/// * `file` - the read of `host`, and (when the bytes arrived) `vpcf_check`'s answer.
///
/// `None` = loadable. `Some(refusal)` = the C's refusal: the log line names `open` and the host
/// path everywhere except the two path-shape refusals, which name the caller's path (`:384-412`).
#[must_use]
pub fn open_gate(
    path: &[u8],
    app: Option<&str>,
    addon: Option<&str>,
    host: Option<&[u8]>,
    file: Option<&GateFile<'_>>,
) -> Option<Refused> {
    let Some(_app) = app else {
        return Some(Refused::new("open", "no_app_root", path)); // :384-388
    };
    if addon.is_none() {
        return Some(Refused::new("open", "no_addon", path)); // :389-393
    }
    // `:394-400`: unreachable with the caps above (app <= 1023, addon <= 63, path <= 519, all
    // bounded by the caller), and the C reports it as a failed read.
    let Some(host) = host else {
        return Some(Refused::new("open", "read_failed", path));
    };
    let Some(file) = file else {
        // The caller only skips the read when `host` failed, which returned above.
        return Some(Refused::new("open", "read_failed", host));
    };
    match file {
        FileRead::Refused(why) => Some(Refused::new("open", why, host)), // :401-405
        FileRead::Bytes(bytes) => match kv3_check::vpcf_check(bytes) {
            Ok(()) => None,
            Err(refusal) => Some(Refused::new("open", refusal.reason.as_str(), host)), // :406-412
        },
    }
}

/// One gate's file half: the read's answer, and the check's when there were bytes.
pub type GateFile<'a> = FileRead<'a>;

/// `pet_text_path`'s join (`mcp_bridge_pet.inc:825-832`): `<base>\cs2mcpbridge\pet_text.vpcf` in
/// the C's 600-byte buffer. `None` = the path does not fit.
#[must_use]
pub fn text_path(base: &str) -> Option<String> {
    let mut p = Buf::new(600);
    p.add(base.as_bytes());
    p.add(b"\\cs2mcpbridge\\pet_text.vpcf");
    (!p.ovf()).then(|| String::from_utf8_lossy(p.bytes()).into_owned())
}

/// `op_pet_text`'s read failures (`mcp_bridge_pet.inc:923-932`): `why` is `pet_read_vpcf`'s
/// out-parameter, and `too_large` becomes `text_too_large`, anything else `text_read_failed`.
#[must_use]
pub fn text_read_error(why: &str) -> OpErr {
    let reason = if why == "too_large" {
        "text_too_large"
    } else {
        "text_read_failed"
    };
    OpErr::new(-32017, "PrerequisiteFailed", reason)
}

/// `op_pet_text`'s `save_to_file_failed` message (`mcp_bridge_pet.inc:911-916`): the prefix plus
/// the callee's error text, bounded by the C's 192-byte `msg`.
#[must_use]
pub fn save_failure_message(error: Option<&str>) -> String {
    let mut msg: Vec<u8> = Vec::with_capacity(192);
    cpy_append(&mut msg, b"save_to_file_failed", 192);
    if let Some(error) = error {
        cpy_append(&mut msg, b": ", 192);
        cpy_append(&mut msg, error.as_bytes(), 192);
    }
    String::from_utf8_lossy(&msg).into_owned()
}

// ---------------------------------------------------------------------------------------------
// pet.load
// ---------------------------------------------------------------------------------------------

/// `pet.load`'s already-mapped answer (`mcp_bridge_pet.inc:428-430`): nothing is called.
pub fn load_already(out: &mut Buf, module: &str) {
    out.add(b"{\"loaded\":true,\"alreadyLoaded\":true,\"toolIndex\":null,\"module\":");
    out.addjson(module.as_bytes());
    out.add(b"}");
}

/// `pet.load`'s attempted answer (`mcp_bridge_pet.inc:452-456`).
pub fn load_result(out: &mut Buf, loaded: bool, tool_index: i64, module: &str) {
    out.add(b"{\"loaded\":");
    out.addbool(loaded);
    out.add(b",\"alreadyLoaded\":false,\"toolIndex\":");
    out.addi(tool_index);
    out.add(b",\"module\":");
    out.addjson(module.as_bytes());
    out.add(b"}");
}

// ---------------------------------------------------------------------------------------------
// The nine arrays
// ---------------------------------------------------------------------------------------------

/// One of the root node's nine function arrays as the reader found it
/// (`pet_arrays_json`, `mcp_bridge_pet.inc:187-212`).
pub enum TypeArray {
    /// `GetChildCount` answered outside `0..=4096`: `root_child_count_unusable`, and nothing is
    /// written for this type (`:191-194`).
    CountUnusable(i32),
    /// The count and one entry per child; `None` is a child whose `_class` the C refused
    /// (`root_child_unreadable`, which aborts the whole list, `:199-204`).
    Children {
        /// `GetChildCount`'s answer.
        count: i32,
        /// One entry per child read, in index order.
        classes: Vec<Option<String>>,
    },
    /// A type the C never reaches: `pet_arrays_json` returns on the first unusable count or
    /// unreadable child (`:191-194`, `:201-204`), so no later `GetChildCount`/`GetChild` call is
    /// made. The reader fills the rest of the nine slots with this (see `game::read_arrays`).
    Stopped,
}

/// `pet_arrays_json` (`mcp_bridge_pet.inc:187-212`): `[{type,count,classes:[...]}, ...]`.
///
/// Fails closed like the C: an unusable count or an unreadable child aborts the whole object
/// (leaving the partial writes for the caller's rollback).
pub fn arrays_json(out: &mut Buf, arrays: &[TypeArray; ARRAY_TYPES]) -> Result<(), OpErr> {
    out.add(b"[");
    for (t, array) in arrays.iter().enumerate() {
        match array {
            TypeArray::CountUnusable(_) => {
                // :191-194: checked before anything is written for this type.
                return Err(OpErr::new(
                    -32017,
                    "PrerequisiteFailed",
                    "root_child_count_unusable",
                ));
            }
            TypeArray::Stopped => {
                // Unreachable: the failure that made the reader stop is an earlier element and
                // returned above. Refuse rather than write a type the C never reported.
                return Err(OpErr::new(
                    -32017,
                    "PrerequisiteFailed",
                    "root_child_unreadable",
                ));
            }
            TypeArray::Children { count, classes } => {
                if t != 0 {
                    out.add(b",");
                }
                out.add(b"{\"type\":");
                out.addi(t as i64);
                out.add(b",\"count\":");
                out.addi(i64::from(*count));
                out.add(b",\"classes\":[");
                for i in 0..*count {
                    let Some(Some(class)) = classes.get(i as usize) else {
                        return Err(OpErr::new(
                            -32017,
                            "PrerequisiteFailed",
                            "root_child_unreadable",
                        ));
                    };
                    if i != 0 {
                        out.add(b",");
                    }
                    out.addjson(class.as_bytes());
                }
                out.add(b"]}");
            }
        }
    }
    out.add(b"]");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The document object (pet.active / pet.open) and the live tail
// ---------------------------------------------------------------------------------------------

/// The document header `pet_doc_json` writes first (`mcp_bridge_pet.inc:221-228`): the session's
/// `GetPath` (a cut prefix when it did not fit, `""` when unreadable - `md_str`'s contract), the
/// dirty flag and the untitled flag.
pub struct DocHead {
    /// `GetPath` (`:222`), the game's own bytes.
    pub path: Vec<u8>,
    /// `IsModified` (`:223`).
    pub dirty: bool,
    /// `m_bIsNew` (`:227`).
    pub is_new: bool,
}

/// What the reader saw for the active document, for `pet_doc_json`
/// (`mcp_bridge_pet.inc:217-246`).
///
/// One value per call, never collected, so the root variant's nine arrays are not worth a box.
#[allow(clippy::large_enum_variant)]
pub enum DocJson {
    /// `session->m_pDoc == 0` (`:219`): nothing is written, reason `no_document`.
    NoDoc,
    /// A document with no root - the empty or untitled state (`:224`): `hasRoot:false`,
    /// `arrays:null`.
    NoRoot(DocHead),
    /// `m_pRoot` is not the pinned `CPetRootNode` (`:233-237`): the object is dropped, reason
    /// `bad_root`.
    BadRoot(DocHead),
    /// A usable document (`:238-243`).
    Root(DocHead, [TypeArray; ARRAY_TYPES]),
}

impl DocJson {
    /// The reason `pet_doc_json` answers `0` with (`:219`, `:234`), for the callers that turn it
    /// into `{"doc":null,"reason":..}`.
    #[must_use]
    pub fn refuse_reason(&self) -> Option<&'static str> {
        match self {
            DocJson::NoDoc => Some("no_document"),
            DocJson::BadRoot(_) => Some("bad_root"),
            DocJson::NoRoot(_) | DocJson::Root(..) => None,
        }
    }
}

/// `pet_doc_json`'s three-way return (`mcp_bridge_pet.inc:214-217`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocOutcome {
    /// `1`: the object was written.
    Written,
    /// `0`: no doc to write (`no_document` / `bad_root`), nothing left in the buffer.
    Refused,
}

/// `pet_doc_json` (`mcp_bridge_pet.inc:217-246`).
pub fn doc_json(out: &mut Buf, doc: &DocJson) -> Result<DocOutcome, OpErr> {
    let DocJson::NoDoc = doc else {
        let (head, root) = match doc {
            DocJson::NoRoot(head) => (head, None),
            DocJson::BadRoot(head) => (head, Some(false)),
            DocJson::Root(head, _arrays) => (head, Some(true)),
            DocJson::NoDoc => unreachable!("the let-else above took every NoDoc"),
        };
        let mark = out.mark(); // :218
        out.add(b"{\"path\":");
        out.addjson(&head.path);
        out.add(b",\"dirty\":");
        out.addbool(head.dirty);
        out.add(b",\"isNewFile\":");
        out.addbool(head.is_new);
        out.add(b",\"hasRoot\":");
        out.addbool(root.is_some());
        out.add(b",\"arrays\":");
        match (root, doc) {
            (None, _) => out.add(b"null"), // :230-232
            (Some(false), _) => {
                // :233-237: the root is not the pinned class - roll the whole object back.
                out.rollback(mark);
                return Ok(DocOutcome::Refused);
            }
            (Some(true), DocJson::Root(_, arrays)) => {
                if let Err(e) = arrays_json(out, arrays) {
                    out.rollback(mark); // :239-242
                    return Err(e);
                }
            }
            (Some(true), _) => unreachable!("only DocJson::Root has arrays"),
        }
        out.add(b",\"docType\":\"vpcf\"}"); // :244
        return Ok(DocOutcome::Written);
    };
    // :219: nothing is written and the caller reports `no_document`.
    Ok(DocOutcome::Refused)
}

/// `pet_live_tail` (`mcp_bridge_pet.inc:877-893`): the dirty/undo flags, then the nine arrays;
/// an unreadable child rolls only the array list back and answers `arrays:null` plus
/// `arraysError`, because a retry would add a second undo step.
pub fn live_tail_json(
    out: &mut Buf,
    dirty: bool,
    can_undo: bool,
    can_redo: bool,
    arrays: &[TypeArray; ARRAY_TYPES],
) {
    out.add(b",\"dirty\":");
    out.addbool(dirty);
    out.add(b",\"canUndo\":");
    out.addbool(can_undo);
    out.add(b",\"canRedo\":");
    out.addbool(can_redo);
    out.add(b",\"arrays\":");
    let mark = out.mark(); // :886
    if let Err(e) = arrays_json(out, arrays) {
        out.rollback(mark); // :888-890
        out.add(b"null,\"arraysError\":");
        out.add(e.data.as_bytes());
    }
}

// ---------------------------------------------------------------------------------------------
// pet.functionTypes
// ---------------------------------------------------------------------------------------------

/// One registry row as the reader found it (`pet_type_row`, `mcp_bridge_pet.inc:539-581`).
pub enum TypeRow {
    /// One of the C's `goto bad` conditions held: a null or unreadable definition, a cut or
    /// unreadable name/class name, an unusable metadata count or range, or an unreadable
    /// metadata name. The whole reply is refused (`function_definition_unreadable`).
    Unreadable,
    /// Every hop was readable and every string copied whole.
    Row {
        /// `IParticleFunctionDefinition::GetName` (`:543`).
        name: String,
        /// `SchemaClassInfoData_t::m_pName` (`:552`).
        class_name: String,
        /// The class's static metadata names, in order (`:565-573`).
        metadata: Vec<String>,
    },
}

/// `pet_type_row` (`mcp_bridge_pet.inc:539-581`).
pub fn type_row_json(
    out: &mut Buf,
    row: &TypeRow,
    t: i32,
    i: i32,
    first: bool,
) -> Result<(), OpErr> {
    let TypeRow::Row {
        name,
        class_name,
        metadata,
    } = row
    else {
        // `:577-580`: every check precedes the row's first write, so a bad row leaves nothing
        // behind to roll back (the C rolls back to `mark` all the same).
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "function_definition_unreadable",
        ));
    };
    if !first {
        out.add(b","); // :558
    }
    out.add(b"{\"type\":");
    out.addi(i64::from(t));
    out.add(b",\"index\":");
    out.addi(i64::from(i));
    out.add(b",\"name\":");
    out.addjson(name.as_bytes());
    out.add(b",\"className\":");
    out.addjson(class_name.as_bytes());
    out.add(b",\"metadata\":[");
    let mut obsolete = false;
    for (k, entry) in metadata.iter().enumerate() {
        if k != 0 {
            out.add(b","); // :570
        }
        out.addjson(entry.as_bytes());
        if entry == "MObsoleteParticleFunction" {
            obsolete = true; // :572
        }
    }
    out.add(b"],\"obsolete\":");
    out.addbool(obsolete);
    out.add(b",\"singleton\":null}"); // :574-575
    Ok(())
}

/// `op_pet_function_types`' object (`mcp_bridge_pet.inc:617-632`).
///
/// `counts` is the reader's `GetFunctionDefinitionCount` answer per type 0..6, and `rows[t]` the
/// definitions it could read for that type (short only when the last one was
/// [`TypeRow::Unreadable`], which aborts the reply).
pub fn function_types_json(
    out: &mut Buf,
    counts: &[i32; FUNCTION_TYPES],
    rows: &[Vec<TypeRow>; FUNCTION_TYPES],
) -> Result<(), OpErr> {
    let mut total: i64 = 0;
    for count in counts {
        if *count < 0 || *count > 4096 {
            // :609-613: checked before the first write.
            return Err(OpErr::new(
                -32017,
                "PrerequisiteFailed",
                "function_count_unusable",
            ));
        }
        total += i64::from(*count);
    }
    out.add(b"{\"count\":");
    out.addi(total);
    out.add(b",\"types\":[");
    let mut first = true;
    for (t, type_rows) in rows.iter().enumerate() {
        for (i, row) in type_rows.iter().enumerate() {
            type_row_json(out, row, t as i32, i as i32, first)?; // :624-627
            first = false;
        }
    }
    out.add(b"]}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// pet.schema
// ---------------------------------------------------------------------------------------------

/// One field of the flattened class chain (`pet_schema_fields`, `mcp_bridge_pet.inc:639-662`).
pub struct SchemaField {
    /// `m_pName` (`:648`).
    pub name: String,
    /// `CSchemaType::m_sTypeName` (`:649-654`): `None` when either hop was unreadable, which the
    /// C writes as `null`.
    pub type_name: Option<String>,
    /// The class the field was declared in (`declaredIn`, `:658`).
    pub declared_in: String,
}

/// A class and its flattened chain, as the reader walked it (`pet_schema_walk` +
/// `pet_schema_bases` + `pet_schema_metadata`, `mcp_bridge_pet.inc:664-717`).
pub struct ClassSchema {
    /// `m_nSizeOf` (`:769`).
    pub size: i32,
    /// `m_pProjectName` (`:766`): `None` when unreadable or cut, which the C writes as `null`.
    pub project: Option<String>,
    /// The flattened base chain in field-walk order, excluding the class itself (`:682-697`).
    pub bases: Vec<String>,
    /// Own fields then the bases' fields, depth-first (`:639-677`).
    pub fields: Vec<SchemaField>,
    /// The class's own static metadata names (`:699-717`).
    pub metadata: Vec<String>,
}

/// What `op_pet_schema`'s read saw (`mcp_bridge_pet.inc:752-782`).
pub enum SchemaRead {
    /// `FindRawClassBinding` answered nothing, or a class whose `m_pName` is not the name
    /// asked for (`:754-758`).
    NotFound,
    /// A hop in the field/base/metadata walk was unreadable (`bad_schema`, `:779-782`).
    Unreadable,
    /// The class and its chain.
    Found(ClassSchema),
}

/// `op_pet_schema`'s object (`mcp_bridge_pet.inc:759-778`).
pub fn schema_json(out: &mut Buf, class: &str, read: &SchemaRead) -> Result<(), OpErr> {
    let found = match read {
        SchemaRead::NotFound => false,
        SchemaRead::Found(_) => true,
        SchemaRead::Unreadable => {
            // `:779-782`: nothing of this reply survives, so the check precedes the writes.
            return Err(OpErr::new(
                -32023,
                "SchemaUnavailable",
                "particles.dll schema walk hit an unreadable hop",
            ));
        }
    };
    out.add(b"{\"class\":");
    out.addjson(class.as_bytes());
    out.add(b",\"found\":");
    out.addbool(found);
    let SchemaRead::Found(schema) = read else {
        out.add(b",\"size\":null,\"project\":null,\"bases\":[],\"fields\":[],\"metadata\":[]}"); // :761-763
        return Ok(());
    };
    out.add(b",\"size\":");
    out.addi(i64::from(schema.size));
    out.add(b",\"project\":");
    match &schema.project {
        Some(project) => out.addjson(project.as_bytes()), // :770
        None => out.add(b"null"),
    }
    out.add(b",\"bases\":[");
    for (n, base) in schema.bases.iter().enumerate() {
        if n != 0 {
            out.add(b","); // :692
        }
        out.addjson(base.as_bytes());
    }
    out.add(b"],\"fields\":[");
    for (n, field) in schema.fields.iter().enumerate() {
        if n != 0 {
            out.add(b","); // :655
        }
        out.add(b"{\"name\":");
        out.addjson(field.name.as_bytes());
        out.add(b",\"type\":");
        match &field.type_name {
            Some(name) => out.addjson(name.as_bytes()), // :657
            None => out.add(b"null"),
        }
        out.add(b",\"declaredIn\":");
        out.addjson(field.declared_in.as_bytes());
        out.add(b"}");
    }
    out.add(b"],\"metadata\":[");
    for (i, entry) in schema.metadata.iter().enumerate() {
        if i != 0 {
            out.add(b","); // :711
        }
        out.addjson(entry.as_bytes());
    }
    out.add(b"]}"); // :777
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The remaining method objects
// ---------------------------------------------------------------------------------------------

/// `pet.active`'s two shapes (`mcp_bridge_pet.inc:1133-1153`).
pub enum ActiveRead<'a> {
    /// No usable session: `{"doc":null,"reason":<reason>}` (`:1137-1139`).
    None(&'a str),
    /// A session; a document that `pet_doc_json` refuses becomes the same `{"doc":null,...}`
    /// shape with that refusal's reason (`:1146-1150`).
    Doc(&'a DocJson),
}

/// `pet.active` (`mcp_bridge_pet.inc:1133-1153`): never an error on a missing document.
pub fn active_json(out: &mut Buf, read: &ActiveRead<'_>) -> Result<(), OpErr> {
    match read {
        ActiveRead::None(reason) => {
            out.add(b"{\"doc\":null,\"reason\":");
            out.addjson(reason.as_bytes());
            out.add(b"}");
            Ok(())
        }
        ActiveRead::Doc(doc) => {
            let mark = out.mark(); // :1141
            out.add(b"{\"doc\":");
            match doc_json(out, doc) {
                Err(e) => {
                    out.rollback(mark); // :1145
                    Err(e)
                }
                Ok(DocOutcome::Written) => {
                    out.add(b"}"); // :1151
                    Ok(())
                }
                Ok(DocOutcome::Refused) => {
                    out.rollback(mark); // :1147-1149
                    out.add(b"{\"doc\":null,\"reason\":");
                    out.addjson(doc.refuse_reason().unwrap_or("?").as_bytes());
                    out.add(b"}");
                    Ok(())
                }
            }
        }
    }
}

/// `pet.open` (`mcp_bridge_pet.inc:521-533`).
pub fn open_json(
    out: &mut Buf,
    opened: bool,
    path: &[u8],
    active: Option<&DocJson>,
) -> Result<(), OpErr> {
    out.add(b"{\"opened\":");
    out.addbool(opened);
    out.add(b",\"path\":");
    out.addjson(path);
    out.add(b",\"active\":");
    let mark = out.mark(); // :524
    match active {
        None => out.add(b"null"), // :530-532
        Some(doc) => match doc_json(out, doc) {
            Err(e) => {
                out.rollback(mark); // :528
                return Err(e);
            }
            Ok(DocOutcome::Written) => {}
            Ok(DocOutcome::Refused) => {
                out.rollback(mark); // :529
                out.add(b"null");
            }
        },
    }
    out.add(b"}");
    Ok(())
}

/// `pet.reload` (`mcp_bridge_pet.inc:1206-1210`).
pub fn reload_json(
    out: &mut Buf,
    reloaded: bool,
    has_root: bool,
    path: &[u8],
    discarded_changes: bool,
) {
    out.add(b"{\"reloaded\":");
    out.addbool(reloaded);
    out.add(b",\"hasRoot\":");
    out.addbool(has_root);
    out.add(b",\"path\":");
    out.addjson(path);
    out.add(b",\"discardedChanges\":");
    out.addbool(discarded_changes);
    out.add(b"}");
}

/// `pet.save` (`mcp_bridge_pet.inc:1240-1243`).
pub fn save_json(out: &mut Buf, saved: bool, skipped_clean: bool, path: &[u8]) {
    out.add(b"{\"saved\":");
    out.addbool(saved);
    out.add(b",\"skippedClean\":");
    out.addbool(skipped_clean);
    out.add(b",\"path\":");
    out.addjson(path);
    out.add(b"}");
}

/// `pet.text`'s members (`mcp_bridge_pet.inc:939-952`).
pub struct TextReply<'a> {
    /// The serialized document (`\"text\"`).
    pub text: &'a [u8],
    /// Its length (`\"bytes\"`, the C's `(long long)len`).
    pub bytes: u64,
    /// The document's own `GetPath` (`\"path\"`), not the private file.
    pub path: &'a [u8],
    /// `IsModified`.
    pub dirty: bool,
    /// `m_bIsNew`.
    pub is_new: bool,
    /// `hist->HasEntries`.
    pub can_undo: bool,
    /// `partner->HasEntries`.
    pub can_redo: bool,
}

/// `pet.text` (`mcp_bridge_pet.inc:939-952`).
///
/// The `text` member is written first, and the C refuses the whole reply with `text_too_large`
/// when that single member overflowed (`:941-944`) - never a cut string.
pub fn text_json(out: &mut Buf, reply: &TextReply<'_>) -> Result<(), OpErr> {
    out.add(b"{\"text\":");
    out.addjson(reply.text);
    if out.ovf() {
        return Err(OpErr::new(
            -32017,
            "PrerequisiteFailed",
            "text_too_large",
        )); // :941-944
    }
    out.add(b",\"bytes\":");
    out.addnum(reply.bytes);
    out.add(b",\"path\":");
    out.addjson(reply.path);
    out.add(b",\"dirty\":");
    out.addbool(reply.dirty);
    out.add(b",\"isNewFile\":");
    out.addbool(reply.is_new);
    out.add(b",\"canUndo\":");
    out.addbool(reply.can_undo);
    out.add(b",\"canRedo\":");
    out.addbool(reply.can_redo);
    out.add(b"}");
    Ok(())
}

/// `pet.apply`'s object (`mcp_bridge_pet.inc:1088-1093`), including [`live_tail_json`].
pub fn apply_json(
    out: &mut Buf,
    label: &str,
    bytes: u64,
    dirty: bool,
    can_undo: bool,
    can_redo: bool,
    arrays: &[TypeArray; ARRAY_TYPES],
) {
    out.add(b"{\"applied\":true,\"label\":");
    out.addjson(label.as_bytes());
    out.add(b",\"bytes\":");
    out.addnum(bytes);
    live_tail_json(out, dirty, can_undo, can_redo, arrays);
    out.add(b"}"); // :1092
}

/// `pet.undo` / `pet.redo` (`mcp_bridge_pet.inc:1105-1118`): `done:false` with the reason is a
/// result, never an error.
pub fn undo_redo_json(
    out: &mut Buf,
    done: bool,
    reason: Option<&str>,
    dirty: bool,
    can_undo: bool,
    can_redo: bool,
    arrays: &[TypeArray; ARRAY_TYPES],
) {
    out.add(b"{\"done\":");
    out.addbool(done);
    if let Some(reason) = reason {
        out.add(b",\"reason\":");
        out.addjson(reason.as_bytes()); // :1108
    }
    live_tail_json(out, dirty, can_undo, can_redo, arrays);
    out.add(b"}");
}

// ---------------------------------------------------------------------------------------------
// Parameter validation: everything the C checks before it touches game memory
// ---------------------------------------------------------------------------------------------

/// `op_pet_open`'s parameter checks (`mcp_bridge_pet.inc:467-509`), in the C's order, ending
/// with the addon-relative path. The G2 host gate runs after this and is [`open_gate`].
pub fn open_params(params: Option<&str>) -> Result<String, OpErr> {
    const SHAPE: &str = "path: relative to the addon content root (particles/...), 1..512 chars, no '..'";
    let params = params.unwrap_or("");
    let json = cstr(params.as_bytes()); // `slen(params)`: the C string, not the buffer
    let mut buf = vec![0u8; 520]; // `char path[520]` (`:467`)
    // `!params || !json_string(params,"path",path,sizeof path) || !path[0]` (`:469`).
    if json.is_empty() || !entity_json::json_string(json, b"path", &mut buf) {
        return Err(OpErr::new(-32602, "InvalidParams", SHAPE));
    }
    let path = cstr(&buf).to_vec();
    if path.is_empty() {
        return Err(OpErr::new(-32602, "InvalidParams", SHAPE));
    }
    if path.len() > 512 {
        // :475
        return Err(OpErr::new(-32602, "InvalidParams", "path: 1..512 chars"));
    }
    // `:476-486`: `json_string` collapses escapes, so the raw member is scanned for a backslash.
    if let Some(raw) = entity_json::json_find(json, b"path") {
        for &byte in &json[raw + 1..] {
            if byte == 0 || byte == b'"' {
                break;
            }
            if byte == b'\\' {
                return Err(OpErr::new(
                    -32602,
                    "InvalidParams",
                    "path: no JSON escapes (use '/' separators)",
                ));
            }
        }
    }
    for &byte in &path {
        // :487-492
        if byte < 0x20 || byte == 0x7f {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "path: no control characters",
            ));
        }
    }
    let pl = path.len();
    if path[0] == b'/' || path[0] == b'\\' || (pl > 1 && path[1] == b':') {
        // :493-497
        return Err(OpErr::new(
            -32602,
            "InvalidParams",
            "path: relative to the addon content root (particles/...), not absolute",
        ));
    }
    if pl < 6
        || path[pl - 5] != b'.'
        || (path[pl - 4] | 0x20) != b'v'
        || (path[pl - 3] | 0x20) != b'p'
        || (path[pl - 2] | 0x20) != b'c'
        || (path[pl - 1] | 0x20) != b'f'
    {
        // :498-502
        return Err(OpErr::new(-32602, "InvalidParams", "path: a .vpcf file"));
    }
    for i in 0..pl.saturating_sub(1) {
        // :503-509
        if path[i] == b'.' && path[i + 1] == b'.' {
            return Err(OpErr::new(-32602, "InvalidParams", "path: no '..'"));
        }
    }
    Ok(String::from_utf8_lossy(&path).into_owned())
}

/// `op_pet_schema`'s parameter checks (`mcp_bridge_pet.inc:724-737`).
pub fn schema_params(params: Option<&str>) -> Result<String, OpErr> {
    const REQUIRED: &str = "class (string) required, 1..127 chars, [A-Za-z0-9_]";
    let params = params.unwrap_or("");
    // `char clsname[512]` (`:724`) - long enough that a >127-char name is refused, not cut.
    let mut buf = vec![0u8; 512];
    let json = cstr(params.as_bytes());
    if json.is_empty() || !entity_json::json_string(json, b"class", &mut buf) {
        return Err(OpErr::new(-32602, "InvalidParams", REQUIRED));
    }
    let name = cstr(&buf).to_vec();
    if name.is_empty() {
        return Err(OpErr::new(-32602, "InvalidParams", REQUIRED));
    }
    if name.len() > 127 {
        return Err(OpErr::new(-32602, "InvalidParams", "class: 1..127 chars"));
    }
    for &byte in &name {
        if !(byte.is_ascii_alphanumeric() || byte == b'_') {
            return Err(OpErr::new(
                -32602,
                "InvalidParams",
                "class: [A-Za-z0-9_] only",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// `pet.apply`'s validated request: the unescaped document text and the undo step's label
/// (`mcp_bridge_pet.inc:958-1021`).
pub struct ApplyParams {
    /// The unescaped `.vpcf` text (`*outLen` bytes, `:976-980`).
    pub text: Vec<u8>,
    /// The label, `MCP apply` when the request has none (`:1011-1013`).
    pub label: String,
}

/// Why `pet.apply` refused before it touched the document.
pub enum ApplyRefusal {
    /// A `-32602 InvalidParams` object (`:961-1010`).
    Invalid(OpErr),
    /// The KV3 gate refused the text: the C logs this line and answers [`unloadable`]
    /// (`:1016-1021`).
    Unloadable {
        /// The `-32602 UnloadableFile` error.
        error: OpErr,
        /// The `pet: apply refused <why>: pet.apply:text` line.
        log: String,
    },
}

/// `op_pet_apply`'s parameter half (`mcp_bridge_pet.inc:958-1021`), in the C's order:
/// find `text`, unescape it, cap it, validate `label`, then run the same `vpcf_check` the load
/// paths run. Nothing here touches game memory.
pub fn apply_params(params: Option<&str>) -> Result<ApplyParams, ApplyRefusal> {
    const TEXT_REQUIRED: &str = "text (string) required";
    const LABEL: &str =
        "label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash";
    let params = params.unwrap_or("");
    let json = cstr(params.as_bytes());
    let plen = json.len(); // `u64 plen = params ? slen(params) : 0` (`:959`)
    let text_at = match entity_json::json_find(json, b"text") {
        Some(at) if at < plen && json[at] == b'"' => at,
        _ => return Err(ApplyRefusal::Invalid(OpErr::new(-32602, "InvalidParams", TEXT_REQUIRED))),
    };
    if text_at + 1 > plen {
        // :966-969: `json_find` cannot report past the NUL.
        return Err(ApplyRefusal::Invalid(OpErr::new(-32602, "InvalidParams", TEXT_REQUIRED)));
    }
    let src = &json[text_at + 1..]; // `const char *src = tv + 1` (`:965`)
    let mut text = vec![0u8; src.len() + 1]; // `cap = srcLen + 1` (`:971`)
    let tlen = match crate::json_text::unescape(src, &mut text) {
        Ok(len) => len,
        Err(reason) => {
            return Err(ApplyRefusal::Invalid(OpErr::new(
                -32602,
                "InvalidParams",
                reason.as_str(),
            )));
        }
    };
    if tlen == 0 || tlen > PET_TEXT_MAX {
        // :981-985
        return Err(ApplyRefusal::Invalid(OpErr::new(
            -32602,
            "InvalidParams",
            "text: 1..393216 bytes",
        )));
    }
    let mut label = String::from("MCP apply"); // :1011-1013
    if let Some(raw) = entity_json::json_find(json, b"label") {
        // `:987-1009`
        let mut lb = vec![0u8; 512];
        let quoted = raw < plen && json[raw] == b'"';
        if !quoted || !entity_json::json_string(json, b"label", &mut lb) {
            return Err(ApplyRefusal::Invalid(OpErr::new(-32602, "InvalidParams", LABEL)));
        }
        let lb = cstr(&lb).to_vec();
        if lb.is_empty() || lb.len() > PET_LABEL_MAX {
            return Err(ApplyRefusal::Invalid(OpErr::new(-32602, "InvalidParams", LABEL)));
        }
        for &byte in &lb {
            // :1002-1009: `json_find` is substring-based, so the label is re-checked.
            if !(0x20..=0x7e).contains(&byte) || byte == b'"' || byte == b'\\' {
                return Err(ApplyRefusal::Invalid(OpErr::new(-32602, "InvalidParams", LABEL)));
            }
        }
        label = String::from_utf8_lossy(&lb).into_owned();
    }
    // `:1014-1021`: the same grammar + root-class gate the load paths run - both loader steps
    // below wedge on malformed input.
    if let Err(refusal) = kv3_check::vpcf_check(&text[..tlen]) {
        let reason = refusal.reason.as_str();
        return Err(ApplyRefusal::Unloadable {
            error: unloadable(reason, b"pet.apply:text"),
            log: log_refusal("apply", reason, b"pet.apply:text"),
        });
    }
    text.truncate(tlen);
    Ok(ApplyParams { text, label })
}

/// `pet.reload` / `pet.save`'s `force:true` (`mcp_bridge_pet.inc:1162-1164`, `:1231-1233`):
/// the C reads the member's first byte and accepts only `t`.
#[must_use]
pub fn force_flag(params: Option<&str>) -> bool {
    let Some(params) = params else {
        return false;
    };
    match entity_json::json_find(params.as_bytes(), b"force") {
        Some(at) => params.as_bytes().get(at) == Some(&b't'),
        None => false,
    }
}
