//! JSON string-body unescaping - the Rust port of `bridge/json_text.h` (m2.13 WP-B1).
//!
//! `pet.apply` carries the whole document as one JSON string; this is the exact inverse of the
//! escape set the Rust client's serde_json emits: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`.
//! Anything else is refused, never guessed - a lone surrogate, a `\u0000`, a `\uXXXX` above
//! 0x7F (serde writes non-ASCII as raw UTF-8, so an escaped non-ASCII is a different producer),
//! an unknown escape, an unterminated string, and a raw byte < 0x20 inside the string all fail
//! closed. The caller sizes the output from the escaped length (unescaping never grows: every
//! escape is at least one byte in, one byte out).
//!
//! This is a line-for-line port of the C (`bridge/json_text.h`, 113 lines); the C output is the
//! spec and the differential corpus test (`crates/cs2-bridge/tests/json_text.rs`) proves the
//! bytes identical case by case. The Rust API replaces the C's raw pointers with slices: where
//! the C takes `(src, srcLen)` and `(out, outCap)` this takes `&[u8]` and `&mut [u8]`, and where
//! the C writes a reason string the Rust returns a [`Reason`]. Everything the C test observes
//! carries over unchanged, including the terminating NUL: on accept [`unescape`] writes `out`
//! `[0..len]` **and** the NUL at `out[len]` (`bridge/json_text.h:60` - the text goes to C-string
//! APIs such as tier0's LoadKV3), so `out` must have room for one byte more than the decoded
//! text, exactly like the C.
//!
//! No static mutable state: re-entrant, like the C header (`bridge/json_text.h:14`).

/// Why [`unescape`] refused an input - the exact reason strings of `bridge/json_text.h:24-30`.
///
/// [`Reason::as_str`] returns those strings; the differential corpus compares this against what
/// the C harness recorded, character for character.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    /// No closing quote within `srcLen`, a trailing backslash, or a `\uXXXX` with fewer than
    /// four hex digits left in the input (`json_text.h:24`, written at `:55`, `:67`, `:81`).
    Unterminated,
    /// An unknown escape, or a bad hex digit in `\uXXXX` (`json_text.h:25`, `:88`, `:100`).
    BadEscape,
    /// `\u0000` (`json_text.h:26`, `:92`).
    Nul,
    /// `\uXXXX` with XXXX > 0x7F, except a lone surrogate (`json_text.h:27`, `:94`).
    NonAsciiEscape,
    /// `\uXXXX` in 0xD800..0xDFFF (`json_text.h:28`, `:94`).
    Surrogate,
    /// A raw byte < 0x20 inside the string (`json_text.h:29`, `:103`).
    ControlByte,
    /// A decoded byte or the terminating NUL would exceed the output capacity
    /// (`json_text.h:30`, `:59`, `:106`).
    TooLong,
}

impl Reason {
    /// The C reason string this variant mirrors (`bridge/json_text.h:24-30`); the differential
    /// corpus compares byte for byte against what the C harness wrote.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Unterminated => "unterminated",
            Reason::BadEscape => "bad_escape",
            Reason::Nul => "nul",
            Reason::NonAsciiEscape => "non_ascii_escape",
            Reason::Surrogate => "surrogate",
            Reason::ControlByte => "control_byte",
            Reason::TooLong => "too_long",
        }
    }
}

/// Returns the index, within `src`, of the closing quote of the string whose body starts at
/// `src` (i.e. `src` is the byte after the opening quote), or [`None`] when there is none. Used
/// for sizing. An escaped quote (`\"`) does not end the string.
///
/// Port of `json_text_end` (`bridge/json_text.h:36-49`); the C's 0 return is [`None`], the
/// pointer is the index.
pub fn end(src: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == b'\\' {
            // the byte after an escape is never the closing quote (json_text.h:40)
            if i + 1 >= src.len() {
                return None;
            }
            i += 2;
            continue;
        }
        if c == b'"' {
            return Some(i); // json_text.h:43
        }
        i += 1;
    }
    None // json_text.h:48
}

/// Unescapes the JSON string body `src` into `out`.
///
/// * `Ok(len)`: the decoded text is `out[..len]`, and `out[len]` is the NUL terminator the C
///   writes for the C-string APIs (`json_text.h:59-60`) - so the caller must size `out` at
///   `len + 1`; unescaping never grows, so the escaped length is a safe upper bound.
/// * `Err(reason)`: refused, never guessed. Like the C, the bytes decoded before the failure
///   point stay written in `out`; the refusal itself never writes (`json_text.h:59`, `:88`,
///   `:103`, `:106` all return before `:107`).
///
/// Port of `json_text_unescape` (`bridge/json_text.h:51-111`). `src` may contain any bytes -
/// it is binary-safe, and raw bytes >= 0x20 (including invalid UTF-8) pass through unchanged,
/// exactly what serde_json does (`test_json_text.c:74-75`).
pub fn unescape(src: &[u8], out: &mut [u8]) -> Result<usize, Reason> {
    // The C sets *reason = "unterminated" up front and never overwrites it on the accept path
    // (json_text.h:55); in Rust the accept path returns Ok, so only the refusals name a reason.
    let mut i = 0;
    let mut o = 0;
    while i < src.len() {
        let mut c = src[i];
        if c == b'"' {
            // the closing quote: done (json_text.h:58)
            if o >= out.len() {
                return Err(Reason::TooLong); // json_text.h:59
            }
            out[o] = 0;
            return Ok(o); // json_text.h:61-62
        }
        if c == b'\\' {
            i += 1;
            if i >= src.len() {
                return Err(Reason::Unterminated); // json_text.h:67
            }
            let e = src[i];
            i += 1; // json_text.h:68
            c = match e {
                b'"' => b'"',  // json_text.h:70
                b'\\' => b'\\', // json_text.h:71
                b'/' => b'/',  // json_text.h:72
                b'b' => 0x08,  // json_text.h:73 ('\b')
                b'f' => 0x0C,  // json_text.h:74 ('\f')
                b'n' => b'\n', // json_text.h:75
                b'r' => b'\r', // json_text.h:76
                b't' => b'\t', // json_text.h:77
                b'u' => {
                    if i + 4 > src.len() {
                        return Err(Reason::Unterminated); // json_text.h:81
                    }
                    let mut v: u64 = 0;
                    for k in 0..4 {
                        let h = src[i + k];
                        let d = match h {
                            b'0'..=b'9' => u64::from(h - b'0'),   // json_text.h:85
                            b'a'..=b'f' => u64::from(h - b'a') + 10, // json_text.h:86
                            b'A'..=b'F' => u64::from(h - b'A') + 10, // json_text.h:87
                            _ => return Err(Reason::BadEscape),   // json_text.h:88
                        };
                        v = v * 16 + d; // json_text.h:89
                    }
                    i += 4; // json_text.h:91
                    if v == 0 {
                        return Err(Reason::Nul); // json_text.h:92
                    }
                    if v > 0x7F {
                        // json_text.h:93-96
                        return Err(if (0xD800..=0xDFFF).contains(&v) {
                            Reason::Surrogate
                        } else {
                            Reason::NonAsciiEscape
                        });
                    }
                    // v <= 0x7F here, so the u64 fits a u8 (json_text.h:97)
                    v as u8
                }
                _ => return Err(Reason::BadEscape), // json_text.h:100
            };
        } else {
            if c < 0x20 {
                return Err(Reason::ControlByte); // json_text.h:103
            }
            i += 1;
        }
        if o + 1 > out.len() {
            return Err(Reason::TooLong); // json_text.h:106
        }
        out[o] = c; // json_text.h:107
        o += 1;
    }
    Err(Reason::Unterminated) // json_text.h:109
}

#[cfg(test)]
mod tests {
    use super::{end, unescape, Reason};

    /// The shortest happy path: every one-char short escape decodes to its C byte
    /// (`test_json_text.c:185-186`, "unescape_short_escapes").
    #[test]
    fn short_escapes() {
        let mut out = [0xFF; 16];
        assert_eq!(
            unescape(b"\\n\\t\\r\\\"\\\\\\/\"", &mut out),
            Ok(6)
        );
        assert_eq!(&out[..7], b"\n\t\r\"\\/\0");
    }

    /// `\uXXXX` up to 0x7F decodes to that ASCII byte
    /// (`test_json_text.c:187-188`, "unescape_u_ascii").
    #[test]
    fn u_ascii() {
        let mut out = [0xFF; 16];
        assert_eq!(unescape(b"\\u0041\"", &mut out), Ok(1));
        assert_eq!(&out[..2], b"A\0");
    }

    /// Every refusal the C test names, with its exact reason
    /// (`test_json_text.c:189-196`, `:194`).
    #[test]
    fn refusals_match_the_c_reasons() {
        let mut out = [0xFF; 16];
        assert_eq!(unescape(b"\\u0000", &mut out), Err(Reason::Nul));
        assert_eq!(unescape(b"\\u00e9", &mut out), Err(Reason::NonAsciiEscape));
        assert_eq!(unescape(b"\\ud83d", &mut out), Err(Reason::Surrogate));
        assert_eq!(unescape(b"abc", &mut out), Err(Reason::Unterminated));
        assert_eq!(unescape(b"abc\\", &mut out), Err(Reason::Unterminated));
        assert_eq!(unescape(b"a\tb", &mut out), Err(Reason::ControlByte));
        assert_eq!(unescape(b"\\x", &mut out), Err(Reason::BadEscape));
    }

    /// Output-capacity refusals: a byte past `out.len()`, and no room for the terminating NUL
    /// (`test_json_text.c:195-196`).
    #[test]
    fn too_long_and_no_room_for_nul() {
        assert_eq!(unescape(b"abcdef\"", &mut [0xFF; 3]), Err(Reason::TooLong));
        // "abc" decodes to 3 bytes but cap 3 leaves no room for out[3] = 0.
        assert_eq!(unescape(b"abc\"", &mut [0xFF; 3]), Err(Reason::TooLong));
        // Exactly enough room is accepted, NUL included.
        let mut out = [0xFF; 3];
        assert_eq!(unescape(b"ab\"", &mut out), Ok(2));
        assert_eq!(&out, b"ab\0");
    }

    /// `end` skips escaped quotes and reports `None` without a closing quote
    /// (`test_json_text.c:115-137`).
    #[test]
    fn end_skips_escaped_quotes() {
        assert_eq!(end(b"a\\\"b\\\"c\"junk"), Some(7));
        assert_eq!(end(b"abc"), None);
        // A trailing backslash can never terminate either (json_text.h:41).
        assert_eq!(end(b"a\\\"b\\"), None);
    }

    /// Raw bytes >= 0x20 pass through unchanged, including invalid UTF-8 - the C is byte-based
    /// (`test_json_text.c:74-75`: "Raw bytes >= 0x20 (including UTF-8) pass through").
    #[test]
    fn raw_bytes_pass_through() {
        let mut out = [0xFF; 16];
        assert_eq!(unescape(b"a\x80\xC3\x28z\"", &mut out), Ok(5));
        assert_eq!(&out[..6], b"a\x80\xC3\x28z\0");
    }

    /// The reason strings are the C spec strings, character for character
    /// (`json_text.h:24-30`).
    #[test]
    fn reason_strings_are_the_c_strings() {
        assert_eq!(Reason::Unterminated.as_str(), "unterminated");
        assert_eq!(Reason::BadEscape.as_str(), "bad_escape");
        assert_eq!(Reason::Nul.as_str(), "nul");
        assert_eq!(Reason::NonAsciiEscape.as_str(), "non_ascii_escape");
        assert_eq!(Reason::Surrogate.as_str(), "surrogate");
        assert_eq!(Reason::ControlByte.as_str(), "control_byte");
        assert_eq!(Reason::TooLong.as_str(), "too_long");
    }
}
