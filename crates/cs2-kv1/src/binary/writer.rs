//! The writer half of [`crate::binary`] — a port of upstream `KV1BinarySerializer`.

use super::{NodeType, StringTable};
use crate::{Document, Error, KeyValue, Value};

/// Writes one document into a byte vector.
///
/// Iterative rather than recursive (upstream's visitor recurses): the pending entries live in an
/// explicit stack, so a deeply nested document cannot overflow the stack while it is written.
pub(crate) struct Writer<'a> {
    out: Vec<u8>,
    string_table: Option<&'a mut StringTable>,
}

impl<'a> Writer<'a> {
    /// A writer that resolves keys through `string_table`, or writes them inline when it is
    /// `None`.
    pub(crate) fn new(string_table: Option<&'a mut StringTable>) -> Self {
        Self { out: Vec::new(), string_table }
    }

    /// The bytes written so far.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.out
    }

    /// Writes the root pair and the document's end marker(s).
    pub(crate) fn write_document(&mut self, doc: &Document) -> Result<(), Error> {
        // Upstream's depth counter: it writes one end marker per closed object and a second one
        // when the outermost object (the root) closes.
        let mut open_objects = 0usize;
        let root_is_object = matches!(doc.value, Value::Object(_));
        let mut work = vec![Job::Pair(doc)];

        while let Some(job) = work.pop() {
            match job {
                Job::Pair(pair) => match &pair.value {
                    Value::Object(object) => {
                        self.tag(NodeType::ChildObject);
                        self.key(&pair.key)?;
                        open_objects += 1;
                        work.push(Job::CloseObject);
                        // The stack pops in reverse, so push the entries back to front.
                        for entry in object.entries.iter().rev() {
                            work.push(Job::Pair(entry));
                        }
                    }
                    value => self.value(value, &pair.key)?,
                },
                Job::CloseObject => {
                    self.tag(NodeType::End);
                    open_objects -= 1;
                    if open_objects == 0 {
                        // The root object is closed: the document itself ends here.
                        self.tag(NodeType::End);
                    }
                }
            }
        }

        if !root_is_object {
            // A non-object root writes no object at all, so upstream's `OnObjectEnd` never runs
            // and the document marker would be missing (its reader rejects such a file).
            self.tag(NodeType::End);
        }
        Ok(())
    }

    /// Writes a value entry: tag, key, payload.
    fn value(&mut self, value: &Value, key: &str) -> Result<(), Error> {
        match value {
            // Objects are opened by `write_document`, which sees the pair rather than the value.
            Value::Object(_) => {
                return Err(Error::Binary {
                    offset: self.out.len(),
                    message: "internal error: an object reached the value writer".to_string(),
                });
            }
            Value::String(string) => {
                self.tag(NodeType::String);
                self.key(key)?;
                self.null_terminated(string);
            }
            Value::WideString(string) => {
                self.tag(NodeType::WideString);
                self.key(key)?;
                self.wide_string(string)?;
            }
            Value::Int32(number) => {
                self.tag(NodeType::Int32);
                self.key(key)?;
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Float32(number) => {
                self.tag(NodeType::Float32);
                self.key(key)?;
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Pointer(number) => {
                self.tag(NodeType::Pointer);
                self.key(key)?;
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Color(color) => {
                self.tag(NodeType::Color);
                self.key(key)?;
                // Stream order, so a colour survives a round trip byte for byte.
                self.out.extend_from_slice(color);
            }
            Value::UInt64(number) => {
                self.tag(NodeType::UInt64);
                self.key(key)?;
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Binary(blob) => {
                self.tag(NodeType::BinaryBlob);
                self.key(key)?;
                let length = u32::try_from(blob.len()).map_err(|_| Error::Binary {
                    offset: self.out.len(),
                    message: format!("binary blob of {} bytes does not fit in a uint32 length", blob.len()),
                })?;
                self.out.extend_from_slice(&length.to_le_bytes());
                self.out.extend_from_slice(blob);
            }
            Value::Int64(number) => {
                self.tag(NodeType::Int64);
                self.key(key)?;
                self.out.extend_from_slice(&number.to_le_bytes());
            }
        }
        Ok(())
    }

    /// Writes the key of the entry that follows: an index into the string table, or the key
    /// itself with a NUL terminator.
    fn key(&mut self, key: &str) -> Result<(), Error> {
        let Some(table) = self.string_table.as_deref_mut() else {
            self.null_terminated(key);
            return Ok(());
        };

        let index = table.get_or_add(key).ok_or_else(|| Error::Binary {
            offset: self.out.len(),
            message: format!("read-only string table has no entry for key {key:?}"),
        })?;
        let index = i32::try_from(index).map_err(|_| Error::Binary {
            offset: self.out.len(),
            message: format!("string table index {index} does not fit in an int32"),
        })?;
        self.out.extend_from_slice(&index.to_le_bytes());
        Ok(())
    }

    /// Writes a NUL-terminated UTF-8 string.
    fn null_terminated(&mut self, string: &str) {
        self.out.extend_from_slice(string.as_bytes());
        self.out.push(0);
    }

    /// Writes a UTF-16LE string with Steam's `int32` character count, which *includes* the NUL
    /// terminator — the shape a document without the magic header is read back with.
    fn wide_string(&mut self, string: &str) -> Result<(), Error> {
        let count = i32::try_from(string.encode_utf16().count() + 1).map_err(|_| Error::Binary {
            offset: self.out.len(),
            message: "wide string does not fit in an int32 character count".to_string(),
        })?;
        self.out.extend_from_slice(&count.to_le_bytes());
        for unit in string.encode_utf16().chain(std::iter::once(0)) {
            self.out.extend_from_slice(&unit.to_le_bytes());
        }
        Ok(())
    }

    /// Appends a tag byte.
    fn tag(&mut self, node_type: NodeType) {
        self.out.push(node_type.byte());
    }
}

/// One step of the iterative traversal.
enum Job<'a> {
    /// Write a pair (an object opens, a value is written).
    Pair(&'a KeyValue),
    /// Write the marker that closes the innermost open object.
    CloseObject,
}
