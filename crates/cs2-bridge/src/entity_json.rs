//! The C bridge's **entity JSON region** (`mcp_bridge.c:226-1010`, between the two
//! `entity JSON region` markers) ported to Rust: the byte scanners the m2.15 entity ops depend on.
//!
//! # Why a byte-faithful port
//!
//! The C region is host-tested text: `build.sh:58-78` lifts it verbatim out of the source
//! into a generated header and compiles `test_entity_json.c` against it, so the C test pins
//! the *shipped* scanners. This module is the Rust bridge's copy of that behaviour, and
//! `crates/cs2-bridge/tests/entity_json.rs` holds it to the C two ways:
//!
//! * every case of `test_entity_json.c` is ported one for one (the C's printed case count
//!   is asserted against the Rust test's own count), and the C test is rebuilt and run;
//! * `tests/data/entity_json_corpus.txt` is a differential corpus produced by the committed C
//!   generator (`tests/data/entity_json_gen.c`), which the test rebuilds with `cc` against the
//!   region extracted from `mcp_bridge.c` exactly as `build.sh` extracts it, and then
//!   requires both the C output and this module's output to equal the committed bytes.
//!
//! # What is ported
//!
//! * [`Sb`]: the string builder with the overflow flag, plus [`Sb::addjson`] (JSON escaping with
//!   one U+FFFD per maximal invalid UTF-8 subpart) and [`Sb::addf`] / [`Sb::addi`] / [`Sb::addnum`].
//! * [`utf8_seq`], [`utf8_prefix`]: the UTF-8 sequence validator and the truncation-safe prefix.
//! * [`json_find`] (the depth-blind finder), [`json_find_top`] (top-level member lookup only),
//!   [`json_bal_end`], [`json_value_end`], [`json_string`] / [`json_string_top`].
//! * [`json_num`], [`json_float1`] / [`json_float3`] and their `_at` / `_top` twins, with the
//!   C's finite/inf fences.
//! * [`json_int`] (long-long bound) and [`json_int_at`] / [`json_int_top`] (INT32 bound).
//! * [`json_estr`], [`kv_next`]: escape decoding (`\uXXXX` with surrogate pairs, `\b`, `\f`, `\/`,
//!   unknown escapes keeping the escaped byte) and the keyvalue walker that keeps a number's raw
//!   token text in `nval` and reports an over-long string (`over = 1`) or an unrepresentable
//!   number (`over = 2`) while still parsing the keys after it.
//! * [`batch_op_next`], [`batch_has_create`], [`batch_node_recorded`],
//!   [`batch_record_decision`]: the string/escape-aware `edit.batch` splitter and its undo rules.
//! * the m2.18 prop-edit gates: [`xform_origin_ok`], [`xform_scales_ok`], [`xform_any`],
//!   [`xform_origin_reason`], [`conn_text_ok`], [`conn_array_parse`] / [`ConnSpec`],
//!   [`rm_mode_ok`], [`rm_indices_parse`], [`rm_sort_desc`].
//!
//! # The string model
//!
//! The C works on `const char *` and stops at the first NUL. The Rust functions take `&[u8]` and
//! apply the same rule: every JSON input is read only up to its first NUL byte (the C's
//! terminator), and bytes after it are invisible. Positions are indices into that same slice, so
//! a position is the C's pointer minus the C's base pointer. A `None` from a finder/parser is the
//! C's `0`.
//!
//! The C's NUL-terminator write of `name[n]` / `sval[v]` / `out[n]` is kept where the C does it;
//! the callers here always pass a non-empty buffer, exactly like the C call sites.
//!
//! # The constants
//!
//! [`JSON_INT_MAX`], [`KV_VALUE_MAX`], the `XFORM_*` / `CONN_*` / `RM_*` bounds and the keyvalue
//! kind tags are the C region's own (`mcp_bridge.c:513`, `:630`, `:831-837`) - they are protocol
//! numbers, not ABI offsets, so they live here verbatim.

/// The largest integer [`json_int_at`] accepts: `JSON_INT_MAX` (`mcp_bridge.c:513`), INT32_MAX.
pub const JSON_INT_MAX: i64 = 2147483647;

/// The longest keyvalue string the bridge writes: `KV_VALUE_MAX` (`mcp_bridge.c:630`). Rust caps a
/// keyvalue at 512 B (`hammer-mcp` `MAX_VALUE_BYTES`); the bridge takes <= 1024 and refuses
/// anything longer instead of cutting it.
pub const KV_VALUE_MAX: usize = 1024;

/// `node.setTransform`: each `|origin component| <= XFORM_ORIGIN_MAX` (`mcp_bridge.c:831`).
pub const XFORM_ORIGIN_MAX: i32 = 1048576;

/// `node.setTransform`: each scale in `(0, XFORM_SCALES_MAX]` (`mcp_bridge.c:833`).
pub const XFORM_SCALES_MAX: f32 = 1000.0;

/// `entity.addConnections`: at most this many entries per call (`mcp_bridge.c:834`).
pub const CONN_MAX: usize = 64;

/// `entity.addConnections`: bytes per output/target/input/param string (`mcp_bridge.c:835`).
pub const CONN_STR_MAX: usize = 255;

/// `entity.addConnections`: delay bound in seconds (`mcp_bridge.c:836`).
pub const CONN_DELAY_MAX: f32 = 3600.0;

/// `entity.removeConnections`: at most this many indices (`mcp_bridge.c:837`).
pub const RM_INDEX_MAX: usize = 512;

/// [`kv_next`] kind for a string value (and for `null`, which becomes the empty string).
pub const KV_STRING: i32 = 5;
/// [`kv_next`] kind for `true` / `false` (`ival` carries 1 / 0).
pub const KV_BOOL: i32 = 4;
/// [`kv_next`] kind for an integer number token (`ival`).
pub const KV_INT: i32 = 2;
/// [`kv_next`] kind for a number token with a fraction or exponent (`fval`).
pub const KV_FLOAT: i32 = 3;

/// Reads byte `index` of `s`, answering NUL past the end: the C scanners read a NUL-terminated
/// string, so a read past the last byte is the terminator (and never a continuation byte).
#[inline]
fn at(s: &[u8], index: usize) -> u8 {
    s.get(index).copied().unwrap_or(0)
}

/// The C string `s` denotes: everything before the first NUL byte.
#[inline]
fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&byte| byte == 0) {
        Some(nul) => &s[..nul],
        None => s,
    }
}

#[inline]
fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

/// The C `x >= lo && x <= hi` on a `u32` code point.
#[inline]
fn in_range(value: u32, lo: u32, hi: u32) -> bool {
    (lo..=hi).contains(&value)
}

/// The byte length of the UTF-8 sequence at `s` (`mcp_bridge.c:253-272`): 1..4, bounded by the
/// `avail` bytes the caller can show, or 0 when it is not a valid one - bad lead byte,
/// missing/short continuation, overlong form, a surrogate or beyond U+10FFFF.
///
/// A read past the end of `s` answers NUL, exactly like the C reading the terminator: it is not a
/// continuation byte, so a sequence cut by the end of the input is invalid. (A NUL byte itself is
/// `< 128` and answers 1, as in the C.)
pub fn utf8_seq(s: &[u8], avail: u32) -> u32 {
    let first = at(s, 0);
    if first < 128 {
        return 1;
    }
    let (n, mut cp): (u32, u32) = if in_range(u32::from(first), 194, 223) {
        (2, u32::from(first) & 31)
    } else if in_range(u32::from(first), 224, 239) {
        (3, u32::from(first) & 15)
    } else if in_range(u32::from(first), 240, 244) {
        (4, u32::from(first) & 7)
    } else {
        return 0;
    };
    if n > avail {
        return 0;
    }
    for i in 1..n as usize {
        let byte = at(s, i);
        if (u32::from(byte) & 192) != 128 {
            return 0;
        }
        cp = (cp << 6) | (u32::from(byte) & 63);
    }
    if n == 2 && cp < 128 {
        return 0; // overlong
    }
    if n == 3 && cp < 2048 {
        return 0;
    }
    if n == 4 && cp < 65536 {
        return 0;
    }
    if in_range(cp, 55296, 57343) {
        return 0; // D800..DFFF is not a code point
    }
    if cp > 1114111 {
        return 0; // U+10FFFF
    }
    n
}

/// The longest prefix of the `n` bytes at `out` that does not end inside a multi-byte UTF-8
/// sequence (`mcp_bridge.c:277-284`): the cut a truncating reader must make.
pub fn utf8_prefix(out: &[u8], n: u32) -> u32 {
    let mut k = n as usize;
    while k > 0 && (at(out, k - 1) & 192) == 128 {
        k -= 1; // walk back over continuations
    }
    if k == 0 {
        return 0; // nothing but continuation bytes
    }
    let avail = n - (k as u32 - 1);
    // The sequence starting at out[k-1] is complete exactly when it uses every remaining byte.
    let mut seq = [0u8; 4];
    for (i, byte) in seq.iter_mut().enumerate() {
        *byte = at(out, k - 1 + i);
    }
    if utf8_seq(&seq, avail) == avail {
        n
    } else {
        k as u32 - 1
    }
}

/// The C region's `SB`: a string builder over a caller-owned buffer with an overflow flag
/// (`mcp_bridge.c:228-245`). Every byte is NUL-terminated, and a write that would need the
/// terminator's byte sets [`Sb::ovf`] instead of cutting the string.
pub struct Sb<'a> {
    buf: &'a mut [u8],
    len: usize,
    ovf: bool,
}

impl<'a> Sb<'a> {
    /// `sb_init` (`mcp_bridge.c:229`): an empty builder over `buf`; a zero-length buffer writes
    /// nothing (the C guards `if (cap) mem[0] = 0`).
    pub fn new(buf: &'a mut [u8]) -> Self {
        if let Some(first) = buf.first_mut() {
            *first = 0;
        }
        Self {
            buf,
            len: 0,
            ovf: false,
        }
    }

    /// The buffer's size in bytes (the C's `cap`): what the string may grow to, one NUL included.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// `sb_putc` (`mcp_bridge.c:230-234`).
    pub fn putc(&mut self, byte: u8) {
        if self.len + 1 >= self.buf.len() {
            self.ovf = true;
            return;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        self.buf[self.len] = 0;
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

    /// `sb_addi` (`mcp_bridge.c:242-245`): a signed value (the error codes are negative).
    ///
    /// The C negates `long long`; [`i64::MIN`] would be UB there and is unreachable from the
    /// region's callers (they pass `-32602` and positive bounds). The magnitude is taken
    /// unsigned here so a caller cannot make it overflow.
    pub fn addi(&mut self, value: i64) {
        if value < 0 {
            self.putc(b'-');
        }
        self.addnum(value.unsigned_abs());
    }

    /// `sb_addf` (`mcp_bridge.c:496-509`): `NaN` / `+-inf` become `null`; otherwise at most three
    /// decimals, rounded half-up, and the C's `u32` integer part.
    ///
    /// The C cast `(u32)v` is UB above `UINT32_MAX`; a Rust float-to-int cast saturates, which is
    /// the closest defined answer. No shipped caller passes a finite value that large (the region
    /// only writes small error numbers and times).
    pub fn addf(&mut self, value: f32) {
        if !value.is_finite() {
            self.add(b"null");
            return;
        }
        let mut v = value;
        if v < 0.0 {
            self.putc(b'-');
            v = -v;
        }
        let mut ip = v as u32;
        let mut frac = ((v - (ip as f32)) * 1000.0 + 0.5) as u32;
        if frac >= 1000 {
            ip += 1;
            frac = 0;
        }
        self.addnum(u64::from(ip));
        if frac != 0 {
            self.putc(b'.');
            self.putc(b'0' + (frac / 100) as u8);
            self.putc(b'0' + ((frac / 10) % 10) as u8);
            self.putc(b'0' + (frac % 10) as u8);
        }
    }

    /// `sb_addjson` (`mcp_bridge.c:289-319`): JSON string escaping. A byte >= 128 is emitted only
    /// when it starts a valid UTF-8 sequence; anything else becomes one U+FFFD per maximal
    /// subpart, because one invalid or cut multibyte sequence makes `serde_json` reject the whole
    /// frame.
    pub fn addjson(&mut self, text: &[u8]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.putc(b'"');
        let mut i = 0;
        while i < text.len() && text[i] != 0 {
            let c = text[i];
            if c == b'"' || c == b'\\' {
                self.putc(b'\\');
                self.putc(c);
                i += 1;
            } else if c == b'\n' {
                self.add(b"\\n");
                i += 1;
            } else if c == b'\r' {
                self.add(b"\\r");
                i += 1;
            } else if c == b'\t' {
                self.add(b"\\t");
                i += 1;
            } else if c < 32 {
                self.add(b"\\u00");
                self.putc(HEX[usize::from(c >> 4)]);
                self.putc(HEX[usize::from(c & 15)]);
                i += 1;
            } else if c < 128 {
                self.putc(c);
                i += 1;
            } else {
                let n = utf8_seq(&text[i..], 4);
                if n != 0 {
                    for j in 0..n as usize {
                        self.putc(at(text, i + j));
                    }
                    i += n as usize;
                } else {
                    // One U+FFFD per maximal subpart: the lead byte plus the continuation bytes
                    // that could belong to its sequence.
                    self.add(b"\xEF\xBF\xBD");
                    let want = if in_range(u32::from(c), 194, 223) {
                        2
                    } else if in_range(u32::from(c), 224, 239) {
                        3
                    } else if in_range(u32::from(c), 240, 244) {
                        4
                    } else {
                        1
                    };
                    let mut got = 1;
                    while got < want && (at(text, i + got) & 192) == 128 {
                        got += 1;
                    }
                    i += got;
                }
            }
        }
        self.putc(b'"');
    }

    /// The bytes written so far, without the terminator.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// `out->len--; out->buf[out->len] = 0` (`mcp_bridge.c:1883`, `sb_add_doc_tail`): drop
    /// everything after the first `len` bytes, with the terminator moved down.
    ///
    /// A `len` at or past the current length keeps the builder unchanged, and the overflow flag is
    /// left alone: the C's rewind does not clear `ovf` either, so a builder that already reported
    /// overflow still answers `-32603` after the tail is spliced in.
    pub fn truncate(&mut self, len: usize) {
        if len < self.len {
            self.len = len;
            self.buf[self.len] = 0;
        }
    }

    /// The builder's buffer capacity (the C's `out->cap`; the hooks lane's
    /// `sound.drain` rollback rule reads it, `mcp_bridge_game.inc:1184`).
    pub fn cap(&self) -> usize {
        self.buf.len()
    }

    /// The hooks lane's per-event rollback (`mcp_bridge_game.inc:1185`):
    /// `out->len = mark; out->buf[mark] = 0; out->ovf = 0;` — unlike [`Sb::truncate`], the
    /// overflow flag is cleared, because the rolled-back event's partial write is what set
    /// it and the drain keeps writing the reply tail afterwards.
    pub fn rewind(&mut self, len: usize) {
        self.truncate(len);
        self.ovf = false;
    }

    /// The C `b->ovf`.
    pub fn ovf(&self) -> bool {
        self.ovf
    }

    /// The bytes written so far as a C string view.
    pub fn as_cstr(&self) -> &[u8] {
        cstr(self.bytes())
    }
}

/// `json_find` (`mcp_bridge.c:328-344`): the depth-blind member finder. It searches the whole
/// text at any depth, strings included, and stays the finder for the `pet.*` / `modeldoc.*` /
/// `game.*` arms. Answers the index of the member's value (after `:` and spaces/tabs).
pub fn json_find(json: &[u8], key: &[u8]) -> Option<usize> {
    let s = cstr(json);
    let mut p = 0;
    while p < s.len() {
        if s[p] != b'"' {
            p += 1;
            continue;
        }
        let mut q = p + 1;
        let mut k = 0;
        while k < key.len() && at(s, q) == key[k] {
            q += 1;
            k += 1;
        }
        if k != key.len() || at(s, q) != b'"' {
            p += 1;
            continue;
        }
        q += 1;
        while at(s, q) == b' ' || at(s, q) == b'\t' {
            q += 1;
        }
        if at(s, q) != b':' {
            p += 1;
            continue;
        }
        q += 1;
        while at(s, q) == b' ' || at(s, q) == b'\t' {
            q += 1;
        }
        return Some(q);
    }
    None
}

/// `json_bal_end` (`mcp_bridge.c:348-359`): the end (exclusive) of the balanced JSON
/// object/array at `start`, or `None` when the text ends first. A brace or bracket inside a
/// string does not count, and an unterminated string is `None`.
pub fn json_bal_end(s: &[u8], start: usize) -> Option<usize> {
    let s = cstr(s);
    if at(s, start) != b'{' && at(s, start) != b'[' {
        return None;
    }
    let mut depth: i64 = 0;
    let mut p = start;
    while p < s.len() {
        if s[p] == b'"' {
            p += 1;
            while p < s.len() && s[p] != b'"' {
                if s[p] == b'\\' && p + 1 < s.len() {
                    p += 1;
                }
                p += 1;
            }
            if p >= s.len() {
                return None; // an unterminated string
            }
        } else if s[p] == b'{' || s[p] == b'[' {
            depth += 1;
        } else if s[p] == b'}' || s[p] == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(p + 1);
            }
        }
        p += 1;
    }
    None
}

/// `json_value_end` (`mcp_bridge.c:361-371`): the end (exclusive) of any JSON value at `start` -
/// a string, a nested object/array, or a scalar lexeme. An unterminated object/array answers
/// `start` (the C's "no progress").
pub fn json_value_end(s: &[u8], start: usize) -> usize {
    let s = cstr(s);
    if let Some(end) = json_bal_end(s, start) {
        return end;
    }
    let first = at(s, start);
    if first == b'{' || first == b'[' {
        return start;
    }
    if first == b'"' {
        let mut p = start + 1;
        while p < s.len() && s[p] != b'"' {
            if s[p] == b'\\' && p + 1 < s.len() {
                p += 1;
            }
            p += 1;
        }
        return if p < s.len() { p + 1 } else { p };
    }
    let mut p = start;
    while p < s.len() && s[p] != b',' && s[p] != b'}' && s[p] != b']' {
        p += 1;
    }
    p
}

/// `json_find_top` (`mcp_bridge.c:373-394`): the value of the **top-level** member `key` of the
/// JSON object at `json`, `None` when it has none. It skips nested objects/arrays and string
/// contents, which is what the entity op params need: `keyValues.origin` (a string) must not win
/// over a top-level `origin` array.
pub fn json_find_top(json: &[u8], key: &[u8]) -> Option<usize> {
    let s = cstr(json);
    let mut p = 0;
    while matches!(at(s, p), b' ' | b'\t' | b'\n' | b'\r') {
        p += 1;
    }
    if at(s, p) != b'{' {
        return None;
    }
    p += 1;
    loop {
        while matches!(at(s, p), b' ' | b'\t' | b'\n' | b'\r' | b',') {
            p += 1;
        }
        if at(s, p) != b'"' {
            return None; // '}' or malformed: no such member
        }
        let mut k = 0;
        let mut q = p + 1;
        while k < key.len() && q < s.len() && s[q] != b'"' && s[q] == key[k] {
            q += 1;
            k += 1;
        }
        let matched = k == key.len() && at(s, q) == b'"';
        p = json_value_end(s, p); // past the key string
        while at(s, p) == b' ' || at(s, p) == b'\t' {
            p += 1;
        }
        if at(s, p) != b':' {
            return None;
        }
        p += 1;
        while at(s, p) == b' ' || at(s, p) == b'\t' {
            p += 1;
        }
        if matched {
            return Some(p);
        }
        p = json_value_end(s, p); // past the value (string-aware)
    }
}

/// `json_string_at` (`mcp_bridge.c:397-407`): the JSON string at `start` (the quote itself)
/// copied to `out` with escapes collapsed one byte at a time, then NUL-terminated.
///
/// The C returns 1 for any quote-led value, even when the string is unterminated or `out` fills
/// up; it never reports a truncation. An empty `out` is refused here: the C would write `out[0]`
/// out of bounds, and no shipped caller passes one.
pub fn json_string_at(s: &[u8], start: usize, out: &mut [u8]) -> bool {
    let s = cstr(s);
    if at(s, start) != b'"' || out.is_empty() {
        return false;
    }
    let mut p = start + 1;
    let mut n = 0;
    while p < s.len() && s[p] != b'"' && n + 1 < out.len() {
        if s[p] == b'\\' && p + 1 < s.len() {
            p += 1;
        }
        out[n] = s[p];
        n += 1;
        p += 1;
    }
    out[n] = 0;
    true
}

/// `json_string` (`mcp_bridge.c:408-410`): the depth-blind finder's string.
pub fn json_string(json: &[u8], key: &[u8], out: &mut [u8]) -> bool {
    match json_find(json, key) {
        Some(start) => json_string_at(json, start, out),
        None => false,
    }
}

/// `json_string_top` (`mcp_bridge.c:411-413`): the top-level member's string.
pub fn json_string_top(json: &[u8], key: &[u8], out: &mut [u8]) -> bool {
    match json_find_top(json, key) {
        Some(start) => json_string_at(json, start, out),
        None => false,
    }
}

/// `json_num` (`mcp_bridge.c:419-449`): one JSON number at `start` (sign, digits, fraction,
/// exponent). Answers the position after the token and the value, or `None` when `start` is not
/// a number or the value is not finite.
///
/// `serde_json` writes `|v| >= 1e16` and `|v| < 1e-5` in exponent form (`1e20`, `-1.5e-7`); a
/// bare `e` stays a malformed token. A lexeme with hundreds of digits (or a long exponent)
/// accumulates to `+-inf`, which is refused instead of handing the caller an infinite
/// origin/radius.
pub fn json_num(s: &[u8], start: usize) -> Option<(usize, f64)> {
    let s = cstr(s);
    let mut p = start;
    let mut negative = false;
    if at(s, p) == b'-' {
        negative = true;
        p += 1;
    }
    if !is_digit(at(s, p)) {
        return None;
    }
    let mut value = 0.0f64;
    while is_digit(at(s, p)) {
        value = value * 10.0 + f64::from(at(s, p) - b'0');
        p += 1;
    }
    if at(s, p) == b'.' {
        p += 1;
        let mut fraction = 0.1f64;
        while is_digit(at(s, p)) {
            value += f64::from(at(s, p) - b'0') * fraction;
            fraction *= 0.1;
            p += 1;
        }
    }
    if at(s, p) == b'e' || at(s, p) == b'E' {
        let mut q = p + 1;
        let mut exponent_negative = false;
        if at(s, q) == b'+' {
            q += 1;
        } else if at(s, q) == b'-' {
            exponent_negative = true;
            q += 1;
        }
        if is_digit(at(s, q)) {
            let mut exponent: u32 = 0;
            while is_digit(at(s, q)) {
                if exponent < 1000 {
                    exponent = exponent * 10 + u32::from(at(s, q) - b'0');
                }
                q += 1;
            }
            for _ in 0..exponent {
                value = if exponent_negative {
                    value / 10.0
                } else {
                    value * 10.0
                };
            }
            p = q;
        }
    }
    let out = if negative { -value } else { value };
    // The same test sb_addf uses, so this needs no math.h: NaN or +-inf is refused.
    if !out.is_finite() {
        return None;
    }
    Some((p, out))
}

/// `f_finite` (`mcp_bridge.c:453`): a float that is neither NaN nor `+-inf`.
#[inline]
pub fn f_finite(value: f32) -> bool {
    value.is_finite()
}

/// `json_float3_at` (`mcp_bridge.c:458-473`): `[x,y,z]` number array -> 3 floats. All three are
/// parsed before any is stored, so a refused literal leaves the caller's vector exactly as it
/// was.
pub fn json_float3_at(s: &[u8], start: usize) -> Option<[f32; 3]> {
    let s = cstr(s);
    if at(s, start) != b'[' {
        return None;
    }
    let mut p = start + 1;
    let mut out = [0.0f32; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        while at(s, p) == b' ' {
            p += 1;
        }
        let (next, value) = json_num(s, p)?;
        p = next;
        let narrowed = value as f32;
        if !f_finite(narrowed) {
            return None;
        }
        *slot = narrowed;
        while at(s, p) == b' ' {
            p += 1;
        }
        if i < 2 {
            if at(s, p) != b',' {
                return None;
            }
            p += 1;
        }
    }
    Some(out)
}

/// `json_float3` (`mcp_bridge.c:474-476`): the depth-blind finder's float3.
pub fn json_float3(json: &[u8], key: &[u8]) -> Option<[f32; 3]> {
    match json_find(json, key) {
        Some(start) => json_float3_at(json, start),
        None => None,
    }
}

/// `json_float3_top` (`mcp_bridge.c:477-479`): the top-level member's float3.
pub fn json_float3_top(json: &[u8], key: &[u8]) -> Option<[f32; 3]> {
    match json_find_top(json, key) {
        Some(start) => json_float3_at(json, start),
        None => None,
    }
}

/// `json_float1_at` (`mcp_bridge.c:481-488`): one JSON number (int, decimal or exponent) as a
/// float, refused when it narrows to `+-inf`.
pub fn json_float1_at(s: &[u8], start: usize) -> Option<f32> {
    let (_, value) = json_num(s, start)?;
    let narrowed = value as f32;
    if !f_finite(narrowed) {
        return None;
    }
    Some(narrowed)
}

/// `json_float1` (`mcp_bridge.c:489-491`): the depth-blind finder's float.
pub fn json_float1(json: &[u8], key: &[u8]) -> Option<f32> {
    match json_find(json, key) {
        Some(start) => json_float1_at(json, start),
        None => None,
    }
}

/// `json_float1_top` (`mcp_bridge.c:492-494`): the top-level member's float.
pub fn json_float1_top(json: &[u8], key: &[u8]) -> Option<f32> {
    match json_find_top(json, key) {
        Some(start) => json_float1_at(json, start),
        None => None,
    }
}

/// `json_int` (`mcp_bridge.c:515-535`): the lenient finder's integer. It refuses a lexeme that
/// would overflow `long long` (the check is before the multiply), but has no semantic cap.
pub fn json_int(json: &[u8], key: &[u8]) -> Option<i64> {
    let start = json_find(json, key)?;
    let s = cstr(json);
    let mut p = start;
    let mut negative = false;
    if at(s, p) == b'-' {
        negative = true;
        p += 1;
    }
    if !is_digit(at(s, p)) {
        return None;
    }
    let mut value: i64 = 0;
    while is_digit(at(s, p)) {
        let digit = i64::from(at(s, p) - b'0');
        if value > (i64::MAX - digit) / 10 {
            return None;
        }
        value = value * 10 + digit;
        p += 1;
    }
    Some(if negative { -value } else { value })
}

/// `json_int_at` (`mcp_bridge.c:543-558`): a plain decimal integer at `start` - no fraction, no
/// exponent, no quotes - bounded to [`JSON_INT_MAX`] before every multiply. The sign is parsed so
/// `-1` is still a readable out-of-range value rather than a parse error.
pub fn json_int_at(s: &[u8], start: usize) -> Option<i64> {
    let s = cstr(s);
    let mut p = start;
    let mut negative = false;
    if at(s, p) == b'-' {
        negative = true;
        p += 1;
    }
    if !is_digit(at(s, p)) {
        return None;
    }
    let mut value: i64 = 0;
    while is_digit(at(s, p)) {
        let digit = i64::from(at(s, p) - b'0');
        if value > (JSON_INT_MAX - digit) / 10 {
            return None; // would exceed INT32_MAX
        }
        value = value * 10 + digit;
        p += 1;
    }
    if at(s, p) == b'.' || at(s, p) == b'e' || at(s, p) == b'E' {
        return None; // a number lexeme, not an integer
    }
    Some(if negative { -value } else { value })
}

/// `json_int_top` (`mcp_bridge.c:559-561`): the top-level member's bounded integer.
pub fn json_int_top(json: &[u8], key: &[u8]) -> Option<i64> {
    match json_find_top(json, key) {
        Some(start) => json_int_at(json, start),
        None => None,
    }
}

/// The C's hex-digit value, with 99 for "not a hex digit" (`mcp_bridge.c:589-591`).
#[inline]
fn hexval(byte: u8) -> u32 {
    if byte.is_ascii_digit() {
        u32::from(byte - b'0')
    } else if (b'a'..=b'f').contains(&byte) {
        u32::from(byte - b'a') + 10
    } else if (b'A'..=b'F').contains(&byte) {
        u32::from(byte - b'A') + 10
    } else {
        99
    }
}

/// `json_estr` (`mcp_bridge.c:568-625`): one JSON string element at `start` decoded into `out`
/// (up to 4 UTF-8 bytes). Answers `(used, written)`: the input bytes consumed (0 at the closing
/// quote/NUL, so every caller loop advances) and the bytes written.
///
/// Handles the short escapes, `\/` and `\uXXXX` with surrogate pairs; a lone surrogate (or
/// `\u0000`, which would end the C string a name/value is read as) becomes U+FFFD, and an
/// unknown escape keeps the escaped byte.
pub fn json_estr(s: &[u8], start: usize, out: &mut [u8; 4]) -> (u32, u32) {
    let s = cstr(s);
    let c = at(s, start);
    if c == 0 || c == b'"' {
        return (0, 0);
    }
    if c != b'\\' {
        out[0] = c;
        return (1, 1);
    }
    let escape = at(s, start + 1);
    if escape == 0 {
        out[0] = b'\\'; // a trailing backslash: malformed frame
        return (1, 1);
    }
    match escape {
        b'"' | b'\\' | b'/' => {
            out[0] = escape;
            return (2, 1);
        }
        b'b' => {
            out[0] = 8;
            return (2, 1);
        }
        b'f' => {
            out[0] = 12;
            return (2, 1);
        }
        b'n' => {
            out[0] = b'\n';
            return (2, 1);
        }
        b'r' => {
            out[0] = b'\r';
            return (2, 1);
        }
        b't' => {
            out[0] = b'\t';
            return (2, 1);
        }
        b'u' => {}
        _ => {
            out[0] = escape; // unknown escape: the escaped byte
            return (2, 1);
        }
    }
    let mut cp: u32 = 0;
    for i in 0..4 {
        let digit = hexval(at(s, start + 2 + i));
        if digit > 15 {
            out[0] = b'u'; // not \uXXXX: the old `u`
            return (2, 1);
        }
        cp = cp * 16 + digit;
    }
    let mut used = 6u32;
    if in_range(cp, 55296, 56319) && at(s, start + 6) == b'\\' && at(s, start + 7) == b'u' {
        let mut low: u32 = 0;
        let mut ok = true;
        for i in 0..4 {
            let digit = hexval(at(s, start + 8 + i));
            if digit > 15 {
                ok = false;
                break;
            }
            low = low * 16 + digit;
        }
        if ok && in_range(low, 56320, 57343) {
            cp = 65536 + ((cp - 55296) << 10) + (low - 56320);
            used = 12;
        }
    }
    if in_range(cp, 55296, 57343) {
        cp = 65533; // a lone surrogate -> U+FFFD
    }
    if cp > 1114111 {
        cp = 65533;
    }
    if cp == 0 {
        cp = 65533; // \u0000 would end the C string a name/value is read as
    }
    if cp < 128 {
        out[0] = cp as u8;
        (used, 1)
    } else if cp < 2048 {
        out[0] = (192 | (cp >> 6)) as u8;
        out[1] = (128 | (cp & 63)) as u8;
        (used, 2)
    } else if cp < 65536 {
        out[0] = (224 | (cp >> 12)) as u8;
        out[1] = (128 | ((cp >> 6) & 63)) as u8;
        out[2] = (128 | (cp & 63)) as u8;
        (used, 3)
    } else {
        out[0] = (240 | (cp >> 18)) as u8;
        out[1] = (128 | ((cp >> 12) & 63)) as u8;
        out[2] = (128 | ((cp >> 6) & 63)) as u8;
        out[3] = (128 | (cp & 63)) as u8;
        (used, 4)
    }
}

/// The literal at `pos` is exactly `lit` (`mcp_bridge.c:638-641`): the compare stops at the
/// input's NUL, so a frame that ends inside a literal is malformed instead of matching from its
/// first bytes and stepping past the terminator.
#[inline]
fn lit_at(s: &[u8], pos: usize, lit: &[u8]) -> bool {
    lit.iter()
        .enumerate()
        .all(|(i, &byte)| at(s, pos + i) == byte)
}

/// One [`kv_next`] result: the position after the value, the value kind and the values the C
/// writes through its out-parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KvValue {
    /// The position after the value (the C's returned pointer).
    pub next: usize,
    /// [`KV_STRING`], [`KV_BOOL`], [`KV_INT`] or [`KV_FLOAT`].
    pub kind: i32,
    /// The integer value for [`KV_INT`] / [`KV_BOOL`].
    pub ival: i64,
    /// The double value for [`KV_FLOAT`] (the C stores `d`, not the narrowed float).
    pub fval: f64,
    /// 0, 1 (a string value did not fit `sval`) or 2 (a number token the DME setters cannot
    /// hold). The token is consumed either way and the keys after it still parse.
    pub over: i32,
}

/// `kv_next` (`mcp_bridge.c:653-753`): iterate a JSON object of keyvalue pairs at depth 1.
/// Answers the position after the value, or `None` when done.
///
/// Value kinds: [`KV_STRING`] (5), [`KV_BOOL`] (4), [`KV_INT`] (2), [`KV_FLOAT`] (3).
/// `nval` receives the raw number token text exactly as written (`"4"`, `1e20`), and `""` for
/// every other kind. The name is written NUL-terminated into `name` (an over-long name is
/// dropped, not cut) and the string value into `sval` (an over-long value sets `over = 1` and
/// leaves the prefix).
pub fn kv_next(
    input: &[u8],
    pos: usize,
    name: &mut [u8],
    sval: &mut [u8],
    nval: &mut [u8],
) -> Option<KvValue> {
    let s = cstr(input);
    if !nval.is_empty() {
        nval[0] = 0;
    }
    if name.is_empty() || sval.is_empty() {
        // The C writes name[0] / sval[0]; no shipped caller passes a zero-length buffer.
        return None;
    }
    let mut p = pos;
    while matches!(at(s, p), b' ' | b',' | b'\n' | b'\t' | b'\r') {
        p += 1;
    }
    if at(s, p) != b'"' {
        return None;
    }
    p += 1;
    let mut n = 0;
    while at(s, p) != 0 && at(s, p) != b'"' {
        let mut unit = [0u8; 4];
        let (used, written) = json_estr(s, p, &mut unit);
        if used == 0 {
            break;
        }
        if n + (written as usize) < name.len() {
            name[n..n + written as usize].copy_from_slice(&unit[..written as usize]);
            n += written as usize;
        }
        p += used as usize; // an over-long name is dropped, not cut
    }
    name[n] = 0;
    if at(s, p) != b'"' {
        return None;
    }
    p += 1;
    while at(s, p) == b' ' || at(s, p) == b':' {
        p += 1;
    }
    if at(s, p) == b'"' {
        p += 1;
        let mut v = 0;
        let mut long_value = false;
        while at(s, p) != 0 && at(s, p) != b'"' {
            let mut unit = [0u8; 4];
            let (used, written) = json_estr(s, p, &mut unit);
            if used == 0 {
                break;
            }
            if v + written as usize >= sval.len() {
                long_value = true; // keep scanning so later keys parse
            } else {
                sval[v..v + written as usize].copy_from_slice(&unit[..written as usize]);
                v += written as usize;
            }
            p += used as usize;
        }
        sval[v] = 0;
        if at(s, p) == b'"' {
            p += 1;
        }
        return Some(KvValue {
            next: p,
            kind: KV_STRING,
            ival: 0,
            fval: 0.0,
            over: i32::from(long_value),
        });
    }
    // The whole literal, never its first bytes: a truncated one is malformed and the scan stops
    // at the frame's NUL instead of making the old 4/5-byte jump past it.
    if lit_at(s, p, b"true") {
        return Some(KvValue {
            next: p + 4,
            kind: KV_BOOL,
            ival: 1,
            fval: 0.0,
            over: 0,
        });
    }
    if lit_at(s, p, b"false") {
        return Some(KvValue {
            next: p + 5,
            kind: KV_BOOL,
            ival: 0,
            fval: 0.0,
            over: 0,
        });
    }
    if lit_at(s, p, b"null") {
        sval[0] = 0; // null -> empty string
        return Some(KvValue {
            next: p + 4,
            kind: KV_STRING,
            ival: 0,
            fval: 0.0,
            over: 0,
        });
    }
    if at(s, p) == b'-' || is_digit(at(s, p)) {
        let token_start = p;
        let mut negative = false;
        let mut is_float = false;
        let mut value = 0.0f64;
        if at(s, p) == b'-' {
            negative = true;
            p += 1;
        }
        while is_digit(at(s, p)) {
            value = value * 10.0 + f64::from(at(s, p) - b'0');
            p += 1;
        }
        if at(s, p) == b'.' {
            is_float = true;
            p += 1;
            let mut fraction = 0.1f64;
            while is_digit(at(s, p)) {
                value += f64::from(at(s, p) - b'0') * fraction;
                fraction *= 0.1;
                p += 1;
            }
        }
        if at(s, p) == b'e' || at(s, p) == b'E' {
            let mut q = p + 1;
            let mut exponent_negative = false;
            if at(s, q) == b'+' {
                q += 1;
            } else if at(s, q) == b'-' {
                exponent_negative = true;
                q += 1;
            }
            if is_digit(at(s, q)) {
                is_float = true; // an exponent makes it a float token
                let mut exponent: u32 = 0;
                while is_digit(at(s, q)) {
                    if exponent < 1000 {
                        exponent = exponent * 10 + u32::from(at(s, q) - b'0');
                    }
                    q += 1;
                }
                for _ in 0..exponent {
                    value = if exponent_negative {
                        value / 10.0
                    } else {
                        value * 10.0
                    };
                }
                p = q;
            }
        }
        if negative {
            value = -value;
        }
        // The token text is kept for every number token, in-range or not: a String attribute
        // takes the literal text.
        if !nval.is_empty() {
            let mut text_len = p - token_start;
            if text_len >= nval.len() {
                text_len = nval.len() - 1;
            }
            nval[..text_len].copy_from_slice(&s[token_start..token_start + text_len]);
            nval[text_len] = 0;
        }
        if is_float {
            let narrowed = value as f32;
            if !f_finite(narrowed) {
                return Some(KvValue {
                    next: p,
                    kind: KV_FLOAT,
                    ival: 0,
                    fval: 0.0,
                    over: 2,
                });
            }
            return Some(KvValue {
                next: p,
                kind: KV_FLOAT,
                ival: 0,
                fval: value,
                over: 0,
            });
        }
        if value >= 9223372036854775808.0 || value < -9223372036854775808.0 {
            // Past the long long range: the cast would be UB and store a different number.
            return Some(KvValue {
                next: p,
                kind: KV_INT,
                ival: 0,
                fval: 0.0,
                over: 2,
            });
        }
        return Some(KvValue {
            next: p,
            kind: KV_INT,
            ival: value as i64,
            fval: 0.0,
            over: 0,
        });
    }
    None
}

/// One [`batch_op_next`] answer: whether an op object was copied, and the position after the
/// scan (the C's `*pp`, which advances even on the "no op" answers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpStep {
    /// The C's return value: 1 when an op object was copied.
    pub copied: bool,
    /// The C's `*pp`.
    pub next: usize,
}

/// `batch_op_next` (`mcp_bridge.c:758-778`): copy the next `{"method":...,"params":{...}}` op
/// object into `opbuf` and its own params object into `pbuf`. Braces/brackets and the closing
/// quote inside a string do not count.
///
/// A zero-length `opbuf`/`pbuf` is refused as "no op" here; the C would underflow its cap. No
/// shipped caller passes one.
pub fn batch_op_next(input: &[u8], pos: usize, opbuf: &mut [u8], pbuf: &mut [u8]) -> OpStep {
    let s = cstr(input);
    let mut p = pos;
    while at(s, p) == b' ' || at(s, p) == b',' {
        p += 1;
    }
    if at(s, p) != b'{' || opbuf.is_empty() || pbuf.is_empty() {
        return OpStep {
            copied: false,
            next: p,
        };
    }
    let Some(end) = json_bal_end(s, p) else {
        return OpStep {
            copied: false,
            next: p + 1, // truncated: stop, never a cut op
        };
    };
    let mut n = end - p;
    if n >= opbuf.len() {
        n = opbuf.len() - 1;
    }
    opbuf[..n].copy_from_slice(&s[p..p + n]);
    opbuf[n] = 0;
    pbuf[0] = 0;
    let op = &opbuf[..n];
    if let Some(params) = json_find_top(op, b"params") {
        if at(op, params) == b'{' {
            let params_end = json_bal_end(op, params);
            let mut m = params_end.map_or(0, |value| value - params);
            if m + 1 > pbuf.len() {
                m = pbuf.len() - 1;
            }
            if m != 0 {
                pbuf[..m].copy_from_slice(&op[params..params + m]);
                pbuf[m] = 0;
            }
        }
    }
    OpStep {
        copied: true,
        next: end,
    }
}

/// `batch_has_create` (`mcp_bridge.c:787-803`): does the ops array contain an `entity.create`
/// op? The undo-group label depends on it. An empty or unreadable array has no create op, which
/// is the safe direction: nothing was created.
pub fn batch_has_create(ops: &[u8]) -> bool {
    let s = cstr(ops);
    if s.is_empty() {
        return false; // the C starts at ops + 1, which would be past the terminator
    }
    let mut opbuf = [0u8; 8192];
    let mut pbuf = [0u8; 8192];
    let mut p = 1; // the caller skips the opening '['
    loop {
        let before = p;
        let step = batch_op_next(s, p, &mut opbuf, &mut pbuf);
        p = step.next;
        if !step.copied || p == before {
            break;
        }
        let mut method = [0u8; 64];
        if !json_string(&opbuf, b"method", &mut method) {
            continue;
        }
        if cstr(&method) == b"entity.create" {
            return true;
        }
    }
    false
}

/// `batch_node_recorded` (`mcp_bridge.c:810-813`): is this node already recorded in the batch's
/// group?
pub fn batch_node_recorded(ids: &[i32], id: i32) -> bool {
    ids.contains(&id)
}

/// `batch_record_decision` (`mcp_bridge.c:820-824`): should this `setKeyValues` op
/// `RecordNodeTree`? A 1 answer appends the node to the batch's list. Past `cap` the answer is
/// still 1 (record: the safe pre-W2 behaviour) but nothing is remembered.
///
/// `cap` must not exceed `ids.len()` (the C callers pass both from the same array).
pub fn batch_record_decision(ids: &mut [i32], count: &mut usize, cap: usize, id: i32) -> bool {
    if id < 0 || batch_node_recorded(&ids[..*count], id) {
        return false;
    }
    if *count < cap {
        ids[*count] = id;
        *count += 1;
    }
    true
}

/// The region's own string compare (`prm_seq`, `mcp_bridge.c:841-844`).
#[inline]
fn prm_seq(a: &[u8], b: &[u8]) -> bool {
    cstr(a) == cstr(b)
}

/// `prm_copy` (`mcp_bridge.c:846-851`): copy a validated NUL-terminated string of at most
/// `cap - 1` bytes, NUL-terminated.
fn prm_copy(dst: &mut [u8], src: &[u8]) {
    if dst.is_empty() {
        return;
    }
    let n = cstr(src).len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&cstr(src)[..n]);
    dst[n] = 0;
}

/// `xform_origin_ok` (`mcp_bridge.c:855-859`): a `[x,y,z]` that is finite and within
/// `+-`[`XFORM_ORIGIN_MAX`] per component.
pub fn xform_origin_ok(v: &[f32; 3]) -> bool {
    v.iter().all(|&component| {
        f_finite(component)
            && component <= XFORM_ORIGIN_MAX as f32
            && component >= -(XFORM_ORIGIN_MAX as f32)
    })
}

/// `xform_scales_ok` (`mcp_bridge.c:863-867`): each scale finite and in
/// `(0, `[`XFORM_SCALES_MAX`]`]`; 0, negative and NaN are refused.
pub fn xform_scales_ok(s: &[f32; 3]) -> bool {
    s.iter()
        .all(|&scale| f_finite(scale) && scale > 0.0 && scale <= XFORM_SCALES_MAX)
}

/// `xform_any` (`mcp_bridge.c:869-871`): `node.setTransform` must carry at least one of
/// origin/angles/scales.
pub fn xform_any(has_origin: bool, has_angles: bool, has_scales: bool) -> bool {
    has_origin || has_angles || has_scales
}

/// `conn_text_ok` (`mcp_bridge.c:876-885`): one output/target/input/param string - at most
/// [`CONN_STR_MAX`] bytes, no C0 or DEL control byte. `required` also refuses the empty string.
pub fn conn_text_ok(s: &[u8], required: bool) -> bool {
    let text = cstr(s);
    if text.len() > CONN_STR_MAX {
        return false;
    }
    if text.iter().any(|&byte| byte < 32 || byte == 127) {
        return false;
    }
    !(required && text.is_empty())
}

/// The validated form of one `entity.addConnections` entry (`ConnSpec`, `mcp_bridge.c:889-896`):
/// defaults applied (`param ""`, `delay 0`, `timesToFire -1`), each string NUL-terminated.
#[derive(Clone, Copy)]
pub struct ConnSpec {
    output: [u8; CONN_STR_MAX + 1],
    target: [u8; CONN_STR_MAX + 1],
    input: [u8; CONN_STR_MAX + 1],
    param: [u8; CONN_STR_MAX + 1],
    /// Delay in seconds.
    pub delay: f32,
    /// `timesToFire`: `-1` is "always", otherwise a positive count.
    pub times_to_fire: i32,
}

impl Default for ConnSpec {
    fn default() -> Self {
        Self {
            output: [0; CONN_STR_MAX + 1],
            target: [0; CONN_STR_MAX + 1],
            input: [0; CONN_STR_MAX + 1],
            param: [0; CONN_STR_MAX + 1],
            delay: 0.0,
            times_to_fire: -1,
        }
    }
}

impl ConnSpec {
    /// The entry's `output` name.
    pub fn output(&self) -> &[u8] {
        cstr(&self.output)
    }

    /// The entry's `target` name.
    pub fn target(&self) -> &[u8] {
        cstr(&self.target)
    }

    /// The entry's `input` name.
    pub fn input(&self) -> &[u8] {
        cstr(&self.input)
    }

    /// The entry's `param` (empty by default).
    pub fn param(&self) -> &[u8] {
        cstr(&self.param)
    }
}

/// `conn_array_parse` (`mcp_bridge.c:902-964`): parse and validate
/// `entity.addConnections`' `connections` array - 1..`max` entries of
/// `{output, target, input, param?, delay?, timesToFire?}`. Every rule is checked here, before
/// any write; an unknown member is refused instead of dropped. Answers the count, or `None` when
/// the array is refused (the C sets `*count = 0` first and returns 0).
///
/// `out` must have at least `max` entries; the C writes `out[0..max]`.
pub fn conn_array_parse(vp: &[u8], out: &mut [ConnSpec], max: usize) -> Option<usize> {
    assert!(
        out.len() >= max,
        "conn_array_parse: out must hold max entries ({} < {max})",
        out.len()
    );
    let s = cstr(vp);
    if at(s, 0) != b'[' {
        return None;
    }
    let mut p = 1;
    let mut count = 0;
    loop {
        while matches!(at(s, p), b' ' | b',' | b'\n' | b'\t' | b'\r') {
            p += 1;
        }
        if at(s, p) == b']' {
            return if count > 0 { Some(count) } else { None };
        }
        if at(s, p) != b'{' || count >= max {
            return None;
        }
        let spec = &mut out[count];
        *spec = ConnSpec::default();
        let mut has_output = false;
        let mut has_target = false;
        let mut has_input = false;
        p += 1;
        loop {
            let before = p;
            let mut name = [0u8; 64];
            let mut sval = [0u8; CONN_STR_MAX + 1];
            let mut nval = [0u8; 96];
            let value = kv_next(s, p, &mut name, &mut sval, &mut nval)?;
            p = value.next;
            if p <= before {
                return None; // kv_next provably advances
            }
            if value.over != 0 {
                return None; // a cut string/number token: refused
            }
            // The member is dispatched BEFORE the separator is looked at: the last member of an
            // object is followed by '}', so a break-then-dispatch order would silently drop it.
            if prm_seq(&name, b"output") && !has_output {
                if value.kind != KV_STRING || !conn_text_ok(&sval, true) {
                    return None;
                }
                prm_copy(&mut spec.output, &sval);
                has_output = true;
            } else if prm_seq(&name, b"target") && !has_target {
                if value.kind != KV_STRING || !conn_text_ok(&sval, true) {
                    return None;
                }
                prm_copy(&mut spec.target, &sval);
                has_target = true;
            } else if prm_seq(&name, b"input") && !has_input {
                if value.kind != KV_STRING || !conn_text_ok(&sval, true) {
                    return None;
                }
                prm_copy(&mut spec.input, &sval);
                has_input = true;
            } else if prm_seq(&name, b"param") {
                if value.kind != KV_STRING || !conn_text_ok(&sval, false) {
                    return None;
                }
                prm_copy(&mut spec.param, &sval);
            } else if prm_seq(&name, b"delay") {
                if value.kind == KV_INT {
                    // an integer token: 0..3600 s
                    if value.ival < 0 || value.ival > i64::from(CONN_DELAY_MAX as i32) {
                        return None;
                    }
                    spec.delay = value.ival as f32;
                } else if value.kind == KV_FLOAT {
                    // a decimal token (also the exponent form)
                    if !(value.fval >= 0.0 && value.fval <= f64::from(CONN_DELAY_MAX)) {
                        return None;
                    }
                    spec.delay = value.fval as f32;
                } else {
                    return None;
                }
            } else if prm_seq(&name, b"timesToFire") {
                // -1 = always, or a positive count; 0 is not a legal firing count
                if value.kind != KV_INT || value.ival < -1 || value.ival == 0 || value.ival > JSON_INT_MAX {
                    return None;
                }
                spec.times_to_fire = value.ival as i32;
            } else {
                return None; // unknown member
            }
            while matches!(at(s, p), b' ' | b'\n' | b'\t' | b'\r') {
                p += 1;
            }
            if at(s, p) == b',' {
                continue; // the next member (kv_next skips it too)
            }
            if at(s, p) == b'}' {
                p += 1;
                break; // the entry ends
            }
            return None; // malformed object
        }
        if !(has_output && has_target && has_input) {
            return None;
        }
        count += 1;
    }
}

/// `rm_mode_ok` (`mcp_bridge.c:968-970`): exactly one of `indices` / `match`.
pub fn rm_mode_ok(has_indices: bool, has_match: bool) -> bool {
    has_indices != has_match
}

/// `rm_indices_parse` (`mcp_bridge.c:975-991`): parse `entity.removeConnections`' `indices` -
/// 1..`max` distinct non-negative ints, each within the i32 range. Answers the count, or `None`
/// when the array is refused.
pub fn rm_indices_parse(vp: &[u8], out: &mut [i32], max: usize) -> Option<usize> {
    assert!(
        out.len() >= max,
        "rm_indices_parse: out must hold max entries ({} < {max})",
        out.len()
    );
    let s = cstr(vp);
    if at(s, 0) != b'[' {
        return None;
    }
    let mut p = 1;
    let mut count = 0;
    loop {
        while matches!(at(s, p), b' ' | b'\n' | b'\t' | b'\r') {
            p += 1;
        }
        if at(s, p) == b']' {
            return if count > 0 { Some(count) } else { None };
        }
        if count >= max {
            return None;
        }
        let value = json_int_at(s, p)?;
        if !(0..=JSON_INT_MAX).contains(&value) {
            return None;
        }
        if out[..count].contains(&(value as i32)) {
            return None; // duplicate
        }
        out[count] = value as i32;
        count += 1;
        while at(s, p) == b'-' || is_digit(at(s, p)) {
            p += 1; // past the token
        }
        while matches!(at(s, p), b' ' | b'\n' | b'\t' | b'\r') {
            p += 1;
        }
        if at(s, p) == b',' {
            p += 1;
            continue;
        }
        if at(s, p) != b']' {
            return None;
        }
    }
}

/// `rm_sort_desc` (`mcp_bridge.c:994-1000`): descending order (`RemoveConnection` shifts the
/// entries after the removed one, so a set of indices is only valid walked highest-first).
pub fn rm_sort_desc(v: &mut [i32]) {
    for i in 1..v.len() {
        let key = v[i];
        let mut k = i;
        while k > 0 && v[k - 1] < key {
            v[k] = v[k - 1];
            k -= 1;
        }
        v[k] = key;
    }
}

/// `xform_origin_reason` (`mcp_bridge.c:1005-1009`): the `node.setTransform` origin refusal,
/// built from the gate itself - `origin: each |component| <= 1048576`.
pub fn xform_origin_reason(dst: &mut [u8]) {
    let mut builder = Sb::new(dst);
    builder.add(b"origin: each |component| <= ");
    builder.addi(i64::from(XFORM_ORIGIN_MAX));
}

#[cfg(test)]
mod tests {
    use super::Sb;

    /// `sb_add_doc_tail`'s rewind (`mcp_bridge.c:1882-1886`): the builder loses exactly its last
    /// byte, its terminator moves down, and a builder that answered `false` before the rewind
    /// keeps its overflow flag (the C never clears `ovf`).
    #[test]
    fn truncate_rewinds_one_byte_and_keeps_the_overflow_flag() {
        let mut buf = [0u8; 8];
        {
            let mut b = Sb::new(&mut buf);
            b.add(b"{\"a\":1}");
            assert_eq!(b.bytes(), b"{\"a\":1}");
            b.truncate(6);
            assert_eq!(b.bytes(), b"{\"a\":1");
            assert_eq!(b.as_cstr(), b"{\"a\":1");
            assert!(!b.ovf());
            // Everything after the new end is gone (the terminator is written at `len`).
            assert_eq!(b.bytes().len(), 6);
            b.truncate(6);
            assert_eq!(b.bytes(), b"{\"a\":1");
            b.truncate(99);
            assert_eq!(b.bytes(), b"{\"a\":1");
        }

        // The C's rewind does not clear the flag: an overflowed builder stays overflowed.
        let mut small = [0u8; 4];
        let mut b = Sb::new(&mut small);
        b.add(b"abcd");
        assert!(b.ovf());
        b.truncate(2);
        assert_eq!(b.bytes(), b"ab");
        assert!(b.ovf(), "truncate must not clear the C's ovf flag");
    }
}
