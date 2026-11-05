//! Header parsing and codec dispatch (upstream `Datamodel.Load` / `Save` / `RegisterCodec`).
//!
//! Codec contract (both codecs implement exactly this):
//!
//! * `decode(header: &Header, rest: &[u8]) -> Result<Datamodel, Error>` — `rest` is every byte
//!   **after** the header's closing `>` (upstream hands the codec the stream positioned there),
//!   so the codec consumes its own newline / NUL. The returned model carries the header's
//!   encoding/format fields.
//! * `encode(dm: &Datamodel) -> Result<Vec<u8>, Error>` — the whole file, header line included
//!   ([`format_header`]), using `dm.encoding` / `dm.encoding_version`.

use crate::{Datamodel, Error};

pub mod binary;
pub mod keyvalues2;

/// `<!-- dmx encoding {encoding} {encoding_version} format {format} {format_version} -->`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub encoding: String,
    pub encoding_version: i32,
    pub format: String,
    pub format_version: i32,
}

/// Upstream `CodecUtilities.FormatHeader`.
pub fn format_header(encoding: &str, encoding_version: i32, format: &str, format_version: i32) -> String {
    format!("<!-- dmx encoding {encoding} {encoding_version} format {format} {format_version} -->")
}

/// Read the header like upstream `Load_Internal`: bytes up to and including the first `>`
/// (at most 129 of them), matched against
/// `<!-- dmx encoding (\S+) ([0-9]+) format (\S+) ([0-9]+) -->`. Returns the header and the
/// offset of the first byte after `>`.
pub fn parse_header(bytes: &[u8]) -> Result<(Header, usize), Error> {
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().take(129) {
        if b == b'>' {
            end = Some(i + 1);
            break;
        }
    }
    let end = end.ok_or_else(|| Error::Header("no '>' in the first 129 bytes".into()))?;
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| Error::Header("not ASCII".into()))?;
    let bad = || Error::Header(format!("{text:?}"));
    let start = text.find("<!-- dmx encoding ").ok_or_else(bad)?;
    let words: Vec<&str> = text[start + "<!-- dmx encoding ".len()..].split(' ').collect();
    // encoding N format F M -->
    if words.len() != 6 || words[2] != "format" || words[5] != "-->" {
        return Err(bad());
    }
    let num = |s: &str| -> Result<i32, Error> {
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        s.parse().map_err(|_| bad())
    };
    let nonspace = |s: &str| -> Result<String, Error> {
        if s.is_empty() || s.chars().any(char::is_whitespace) { Err(bad()) } else { Ok(s.to_string()) }
    };
    let header = Header {
        encoding: nonspace(words[0])?,
        encoding_version: num(words[1])?,
        format: nonspace(words[3])?,
        format_version: num(words[4])?,
    };
    Ok((header, end))
}

/// Upstream `RegisterCodec` table: which encoding/version pairs exist.
pub fn is_supported(encoding: &str, version: i32) -> bool {
    match encoding {
        "binary" => matches!(version, 1..=5 | 9),
        "keyvalues2" | "keyvalues2_noids" => matches!(version, 1..=4),
        _ => false,
    }
}

/// Decode a whole DMX file.
pub fn load(bytes: &[u8]) -> Result<Datamodel, Error> {
    let (header, end) = parse_header(bytes)?;
    if !is_supported(&header.encoding, header.encoding_version) {
        return Err(Error::Unsupported { encoding: header.encoding, version: header.encoding_version });
    }
    let rest = &bytes[end..];
    if header.encoding == "binary" {
        binary::decode(&header, rest)
    } else {
        keyvalues2::decode(&header, rest)
    }
}

/// Encode with `dm.encoding` / `dm.encoding_version`.
pub fn save(dm: &Datamodel) -> Result<Vec<u8>, Error> {
    if !is_supported(&dm.encoding, dm.encoding_version) {
        return Err(Error::Unsupported { encoding: dm.encoding.clone(), version: dm.encoding_version });
    }
    if dm.encoding == "binary" {
        binary::encode(dm)
    } else {
        keyvalues2::encode(dm)
    }
}
