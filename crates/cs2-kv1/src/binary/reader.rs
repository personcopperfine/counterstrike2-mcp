//! The reader half of [`crate::binary`] — a port of upstream `KV1BinaryReader`.

use super::{NodeType, StringTable, MAGIC_HEADER, MAX_BINARY_DEPTH};
use crate::{Document, Error, KeyValue, Object, Value};

/// Reads one document, or several consecutive ones, from a byte slice.
///
/// Iterative rather than recursive (upstream recurses once per nesting level): the open objects
/// live in an explicit stack, so only [`MAX_BINARY_DEPTH`] bounds the work.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    string_table: Option<&'a StringTable>,
    /// True when the input starts with the `VBKV` marker, which makes a wide string use Source 2's
    /// `int16` character count (upstream `hasMagicHeader`).
    magic_header: bool,
}

impl<'a> Reader<'a> {
    /// A reader positioned at the first document of `bytes`.
    pub(crate) fn new(bytes: &'a [u8], string_table: Option<&'a StringTable>) -> Self {
        Self { bytes, pos: 0, string_table, magic_header: false }
    }

    /// How many bytes have been consumed.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// True when the input is exhausted.
    pub(crate) fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// Looks for the `VBKV` marker at the current position and, when it is there, skips it and its
    /// CRC32 (upstream `DetectMagicHeader`): only a document that has the marker itself uses Source
    /// 2's wide-string convention. Runs once per document, because upstream builds a fresh reader
    /// — and so re-detects — for every `Deserialize` call. Position 8 is the CRC's end.
    fn detect_magic_header(&mut self) {
        self.magic_header = self.bytes.len() - self.pos >= 8
            && u32::from_le_bytes([
                self.bytes[self.pos],
                self.bytes[self.pos + 1],
                self.bytes[self.pos + 2],
                self.bytes[self.pos + 3],
            ]) == MAGIC_HEADER;
        if self.magic_header {
            self.pos += 8;
        }
    }

    /// Reads one document: the root pair plus its end marker.
    ///
    /// A document that starts with an end marker has no root object at all; upstream reads no
    /// callbacks for it and fails later in its object builder, so it is an error here too.
    pub(crate) fn read_document(&mut self) -> Result<Document, Error> {
        self.detect_magic_header();

        let tag_offset = self.pos;
        let tag = self.read_u8()?; // end of input: upstream's EndOfStreamException
        let node_type = self.node_type(tag, tag_offset)?;
        if node_type.is_end() {
            return Err(Error::Binary {
                offset: tag_offset,
                message: "no root object: the document starts with an end marker".to_string(),
            });
        }

        // The tag is checked before the key is read, so an unknown tag is reported at the tag
        // itself (upstream reads the key first; see the module docs).
        let key = self.read_key()?;

        let value = if node_type == NodeType::ChildObject {
            Value::Object(self.read_object_body()?)
        } else {
            self.read_scalar(node_type, tag_offset)?
        };

        // One more marker closes the document itself (upstream `ReadRootObject`). An object root
        // has already consumed its own marker in `read_object_body`, so this is the second one.
        let end_offset = self.pos;
        let end_tag = self.read_u8()?;
        if !self.node_type(end_tag, end_offset)?.is_end() {
            return Err(Error::Binary {
                offset: end_offset,
                message: "found data after the root object: documents with multiple root objects are not supported"
                    .to_string(),
            });
        }
        Ok(KeyValue { key, value, condition: None })
    }

    /// Reads every entry of an object up to the marker that closes it.
    ///
    /// The object the caller already consumed the tag and key for is the outermost frame; the
    /// loop returns it as soon as its own marker arrives, so `stack.remove(0)` below always has a
    /// frame to remove.
    fn read_object_body(&mut self) -> Result<Object, Error> {
        let mut stack: Vec<Frame> = vec![Frame { key: String::new(), entries: Object::new() }];

        loop {
            let tag_offset = self.pos;
            let tag = self.read_u8()?;
            let node_type = self.node_type(tag, tag_offset)?;

            if node_type.is_end() {
                if stack.len() == 1 {
                    return Ok(stack.remove(0).entries);
                }
                // Close the innermost object and append it to its parent.
                if let (Some(frame), Some(parent)) = (stack.pop(), stack.last_mut()) {
                    parent.entries.push(frame.key, Value::Object(frame.entries));
                }
                continue;
            }

            let key = self.read_key()?;

            if node_type == NodeType::ChildObject {
                // Depth counts the open objects: the root object is 1.
                if stack.len() >= MAX_BINARY_DEPTH {
                    return Err(Error::Binary {
                        offset: tag_offset,
                        message: format!(
                            "object nested deeper than {MAX_BINARY_DEPTH} objects (MAX_BINARY_DEPTH)"
                        ),
                    });
                }
                stack.push(Frame { key, entries: Object::new() });
                continue;
            }

            let value = self.read_scalar(node_type, tag_offset)?;
            if let Some(frame) = stack.last_mut() {
                frame.entries.entries.push(KeyValue { key, value, condition: None });
            }
        }
    }

    /// The [`NodeType`] for a tag byte, rejecting unknown ones at the tag's offset.
    ///
    /// Upstream only rejects an unknown tag in the `default:` arm of `ReadValue`, after the key
    /// has been read, so the offset it reports is past the key; this reader checks first (see the
    /// module docs).
    fn node_type(&self, tag: u8, offset: usize) -> Result<NodeType, Error> {
        NodeType::from_byte(tag).ok_or_else(|| Error::Binary {
            offset,
            message: format!("unhandled binary node type 0x{tag:02X}"),
        })
    }

    /// Reads the payload of a value entry.
    fn read_scalar(&mut self, node_type: NodeType, tag_offset: usize) -> Result<Value, Error> {
        Ok(match node_type {
            NodeType::String => Value::String(self.read_null_terminated_utf8()?),
            NodeType::Int32 => Value::Int32(self.read_i32()?),
            NodeType::Float32 => Value::Float32(f32::from_le_bytes(self.read_array::<4>()?)),
            // Source 1 writes pointers as 4 bytes, Steam and Source 2 write them as 8. No known
            // file contains pointers, so only the Source 1 size is supported (upstream).
            NodeType::Pointer => Value::Pointer(self.read_i32()?),
            NodeType::WideString => Value::WideString(self.read_wide_string()?),
            NodeType::Color => Value::Color(self.read_array::<4>()?),
            NodeType::UInt64 => Value::UInt64(u64::from_le_bytes(self.read_array::<8>()?)),
            NodeType::BinaryBlob => Value::Binary(self.read_blob()?),
            NodeType::Int64 => Value::Int64(i64::from_le_bytes(self.read_array::<8>()?)),
            // The two callers have already turned the three non-value tags away, so this arm is
            // unreachable; it is an error rather than a panic, in case that ever changes.
            NodeType::ChildObject | NodeType::End | NodeType::AlternateEnd => {
                return Err(Error::Binary {
                    offset: tag_offset,
                    message: format!("node type 0x{:02X} is not a value", node_type.byte()),
                });
            }
        })
    }

    /// Reads a key: an index into the string table, or a NUL-terminated UTF-8 string.
    fn read_key(&mut self) -> Result<String, Error> {
        let Some(table) = self.string_table else {
            return self.read_null_terminated_utf8();
        };

        let offset = self.pos;
        let index = self.read_i32()?;
        let bad = |message: String| Error::Binary { offset, message };
        if index < 0 {
            return Err(bad(format!("string table index {index} is negative")));
        }
        match table.get(index as usize) {
            Some(key) => Ok(key.to_string()),
            None => Err(bad(format!(
                "string table index {index} is out of range (the table holds {} entries)",
                table.len()
            ))),
        }
    }

    /// Reads up to the next NUL byte, as UTF-8 (invalid sequences become U+FFFD, like .NET's
    /// default decoder fallback). A string that runs to the end of the input is an error:
    /// upstream's `ReadByte` throws `EndOfStreamException` there.
    fn read_null_terminated_utf8(&mut self) -> Result<String, Error> {
        let start = self.pos;
        let end = self.bytes[start..].iter().position(|&byte| byte == 0).ok_or_else(|| {
            Error::Binary {
                offset: start,
                message: "unterminated string: no NUL byte before the end of the input".to_string(),
            }
        })?;
        self.pos = start + end + 1;
        Ok(String::from_utf8_lossy(&self.bytes[start..start + end]).into_owned())
    }

    /// Reads a UTF-16LE string. The count is Source 2's `int16` *characters* when the document
    /// started with the magic header (no terminator) and Steam's `int32` count that *includes*
    /// the terminator otherwise. Everything from the first NUL character on is dropped.
    fn read_wide_string(&mut self) -> Result<String, Error> {
        let length_offset = self.pos;
        let length = if self.magic_header {
            i32::from(i16::from_le_bytes(self.read_array::<2>()?))
        } else {
            self.read_i32()?
        };
        if length < 0 {
            return Err(Error::Binary {
                offset: length_offset,
                message: format!("invalid wide string length {length}"),
            });
        }
        // `take` refuses a count the input cannot satisfy, so a hostile length never allocates
        // (and the multiplication cannot wrap: the count is capped by the bytes that are there).
        let byte_length = (length as usize).checked_mul(2).ok_or_else(|| Error::Binary {
            offset: length_offset,
            message: format!("wide string length {length} overflows a byte count"),
        })?;
        let bytes = self.take(byte_length)?;
        // The slice is an even number of bytes, so there is no remainder.
        let (pairs, _) = bytes.as_chunks::<2>();
        let units = pairs.iter().map(|pair| u16::from_le_bytes(*pair));
        Ok(char::decode_utf16(units)
            .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER))
            .take_while(|&character| character != '\0')
            .collect())
    }

    /// Reads a length-prefixed blob, refusing a length the input cannot satisfy (upstream
    /// allocates first and throws `EndOfStreamException` afterwards).
    fn read_blob(&mut self) -> Result<Vec<u8>, Error> {
        let offset = self.pos;
        let length = u32::from_le_bytes(self.read_array::<4>()?) as usize;
        if length > self.bytes.len() - self.pos {
            return Err(Error::Binary {
                offset,
                message: format!(
                    "binary blob of {length} bytes does not fit in the {} bytes left in the input",
                    self.bytes.len() - self.pos
                ),
            });
        }
        Ok(self.take(length)?.to_vec())
    }

    /// Reads `N` bytes as an array.
    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let slice = self.take(N)?;
        let mut array = [0u8; N];
        array.copy_from_slice(slice);
        Ok(array)
    }

    /// Reads a little-endian `i32`.
    fn read_i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.read_array::<4>()?))
    }

    /// Reads the next byte.
    fn read_u8(&mut self) -> Result<u8, Error> {
        Ok(self.read_array::<1>()?[0])
    }

    /// Consumes `length` bytes.
    fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        let remaining = self.bytes.len() - self.pos;
        if length > remaining {
            return Err(Error::Binary {
                offset: self.pos,
                message: format!("unexpected end of input: {length} more bytes needed, {remaining} left"),
            });
        }
        let slice = &self.bytes[self.pos..self.pos + length];
        self.pos += length;
        Ok(slice)
    }
}

/// An object being read, with the key its parent will store it under.
struct Frame {
    key: String,
    entries: Object,
}
