//! Small, transport-agnostic building blocks for Language Server Protocol support.
//!
//! This module deliberately does not parse JSON or manage a child process.  It owns the
//! two pieces that are easiest to get subtly wrong at those boundaries instead: translating
//! the editor's Unicode-scalar cursor offsets to LSP UTF-16 positions, and splitting the
//! byte stream used by JSON-RPC into complete UTF-8 message bodies.

use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use ropey::Rope;

macro_rules! index_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(usize);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(value: usize) -> Self {
                Self(value)
            }

            pub const fn get(self) -> usize {
                self.0
            }

            pub const fn checked_add(self, amount: usize) -> Option<Self> {
                match self.0.checked_add(amount) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn checked_sub(self, amount: usize) -> Option<Self> {
                match self.0.checked_sub(amount) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }
        }

        impl From<usize> for $name {
            fn from(value: usize) -> Self {
                Self(value)
            }
        }

        impl From<$name> for usize {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

index_type!(Line, "A zero-based logical line number.");
index_type!(
    CharOffset,
    "An absolute offset measured in Unicode scalar values (Rust `char`s)."
);
index_type!(ByteOffset, "An absolute offset measured in UTF-8 bytes.");
index_type!(
    Utf16Offset,
    "A line-relative offset measured in UTF-16 code units, as required by LSP."
);

/// A zero-based LSP position. `character` is a UTF-16 code-unit offset, not a UTF-8 byte
/// offset and not necessarily the editor's scalar-value column.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct LspPosition {
    pub line: Line,
    pub character: Utf16Offset,
}

impl LspPosition {
    pub const fn new(line: Line, character: Utf16Offset) -> Self {
        Self { line, character }
    }
}

/// Why an exact coordinate conversion could not be performed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinateError {
    CharOutOfBounds {
        offset: CharOffset,
        len: CharOffset,
    },
    ByteOutOfBounds {
        offset: ByteOffset,
        len: ByteOffset,
    },
    ByteNotCharBoundary {
        offset: ByteOffset,
    },
    LineOutOfBounds {
        line: Line,
        line_count: usize,
    },
    Utf16OutOfBounds {
        line: Line,
        offset: Utf16Offset,
        line_len: Utf16Offset,
    },
    Utf16InsideSurrogatePair {
        line: Line,
        offset: Utf16Offset,
    },
    CursorInsideLineEnding {
        offset: CharOffset,
    },
}

impl fmt::Display for CoordinateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CharOutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "character offset {offset} exceeds document length {len}"
                )
            }
            Self::ByteOutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "byte offset {offset} exceeds document length {len}"
                )
            }
            Self::ByteNotCharBoundary { offset } => {
                write!(
                    formatter,
                    "byte offset {offset} is not a UTF-8 scalar boundary"
                )
            }
            Self::LineOutOfBounds { line, line_count } => write!(
                formatter,
                "line {line} is outside a document with {line_count} lines"
            ),
            Self::Utf16OutOfBounds {
                line,
                offset,
                line_len,
            } => write!(
                formatter,
                "UTF-16 offset {offset} exceeds line {line}'s length of {line_len} code units"
            ),
            Self::Utf16InsideSurrogatePair { line, offset } => write!(
                formatter,
                "UTF-16 offset {offset} on line {line} splits a surrogate pair"
            ),
            Self::CursorInsideLineEnding { offset } => {
                write!(
                    formatter,
                    "character offset {offset} is inside a CRLF line ending"
                )
            }
        }
    }
}

impl Error for CoordinateError {}

/// An immutable Ropey text snapshot paired with the document version that produced it.
///
/// Cloning a Rope is cheap enough for asynchronous request snapshots: Ropey shares its
/// immutable tree storage until either clone is changed.
#[derive(Clone, Debug)]
pub struct DocumentSnapshot {
    text: Rope,
    version: DocumentVersion,
}

impl DocumentSnapshot {
    pub fn from_text(text: &str, version: DocumentVersion) -> Self {
        Self {
            text: Rope::from_str(text),
            version,
        }
    }

    pub fn from_rope(text: &Rope, version: DocumentVersion) -> Self {
        Self {
            text: text.clone(),
            version,
        }
    }

    pub fn version(&self) -> DocumentVersion {
        self.version
    }

    pub fn rope(&self) -> &Rope {
        &self.text
    }

    pub fn len_chars(&self) -> CharOffset {
        CharOffset::new(self.text.len_chars())
    }

    pub fn len_bytes(&self) -> ByteOffset {
        ByteOffset::new(self.text.len_bytes())
    }

    pub fn line_count(&self) -> usize {
        self.text.len_lines()
    }

    pub fn char_to_byte(&self, offset: CharOffset) -> Result<ByteOffset, CoordinateError> {
        char_to_byte(&self.text, offset)
    }

    pub fn byte_to_char(&self, offset: ByteOffset) -> Result<CharOffset, CoordinateError> {
        byte_to_char(&self.text, offset)
    }

    pub fn char_to_position(&self, offset: CharOffset) -> Result<LspPosition, CoordinateError> {
        char_to_lsp_position(&self.text, offset)
    }

    pub fn position_to_char(&self, position: LspPosition) -> Result<CharOffset, CoordinateError> {
        lsp_position_to_char(&self.text, position)
    }
}

/// Convert an absolute Unicode-scalar offset to an exact UTF-8 byte boundary.
pub fn char_to_byte(text: &Rope, offset: CharOffset) -> Result<ByteOffset, CoordinateError> {
    let len = CharOffset::new(text.len_chars());
    if offset > len {
        return Err(CoordinateError::CharOutOfBounds { offset, len });
    }
    Ok(ByteOffset::new(text.char_to_byte(offset.get())))
}

/// Convert an exact UTF-8 byte boundary to an absolute Unicode-scalar offset.
///
/// Ropey's `byte_to_char` can identify the scalar containing a byte.  LSP/editing cursors
/// must lie *between* scalars, so this function additionally rejects bytes in the middle of
/// a multi-byte encoding.
pub fn byte_to_char(text: &Rope, offset: ByteOffset) -> Result<CharOffset, CoordinateError> {
    let len = ByteOffset::new(text.len_bytes());
    if offset > len {
        return Err(CoordinateError::ByteOutOfBounds { offset, len });
    }

    let char_offset = text.byte_to_char(offset.get());
    if text.char_to_byte(char_offset) != offset.get() {
        return Err(CoordinateError::ByteNotCharBoundary { offset });
    }
    Ok(CharOffset::new(char_offset))
}

/// Convert an absolute Unicode-scalar cursor to an LSP UTF-16 position.
///
/// A cursor on either side of a line ending is valid.  The otherwise representable cursor
/// between `\r` and `\n` is rejected because LSP has no unambiguous line/column spelling for
/// it.  The editor normalizes file text to `\n`, but retaining this check makes standalone
/// snapshots safe as well.
pub fn char_to_lsp_position(
    text: &Rope,
    offset: CharOffset,
) -> Result<LspPosition, CoordinateError> {
    let len = CharOffset::new(text.len_chars());
    if offset > len {
        return Err(CoordinateError::CharOutOfBounds { offset, len });
    }

    let line_index = text.char_to_line(offset.get());
    let line = Line::new(line_index);
    let line_start = text.line_to_char(line_index);
    let content_end = line_content_end_char(text, line_index);
    if offset.get() > content_end {
        return Err(CoordinateError::CursorInsideLineEnding { offset });
    }

    let utf16 = text
        .slice(line_start..offset.get())
        .chars()
        .map(char::len_utf16)
        .sum();
    Ok(LspPosition::new(line, Utf16Offset::new(utf16)))
}

/// Convert an LSP UTF-16 position to an exact absolute Unicode-scalar cursor.
///
/// Positions beyond the visible line or in the middle of a non-BMP scalar's surrogate pair
/// are errors rather than being silently rounded.  This prevents diagnostics and edits from
/// drifting onto adjacent text.
pub fn lsp_position_to_char(
    text: &Rope,
    position: LspPosition,
) -> Result<CharOffset, CoordinateError> {
    let line_index = position.line.get();
    let line_count = text.len_lines();
    if line_index >= line_count {
        return Err(CoordinateError::LineOutOfBounds {
            line: position.line,
            line_count,
        });
    }

    let start = text.line_to_char(line_index);
    let end = line_content_end_char(text, line_index);
    let wanted = position.character.get();
    let mut utf16 = 0;
    let mut char_offset = start;

    if wanted == 0 {
        return Ok(CharOffset::new(start));
    }

    for character in text.slice(start..end).chars() {
        let next_utf16 = utf16 + character.len_utf16();
        if wanted < next_utf16 {
            return Err(CoordinateError::Utf16InsideSurrogatePair {
                line: position.line,
                offset: position.character,
            });
        }
        char_offset += 1;
        utf16 = next_utf16;
        if wanted == utf16 {
            return Ok(CharOffset::new(char_offset));
        }
    }

    Err(CoordinateError::Utf16OutOfBounds {
        line: position.line,
        offset: position.character,
        line_len: Utf16Offset::new(utf16),
    })
}

fn line_content_end_char(text: &Rope, line: usize) -> usize {
    let start = text.line_to_char(line);
    let mut end = if line + 1 < text.len_lines() {
        text.line_to_char(line + 1)
    } else {
        text.len_chars()
    };

    if end > start && text.char(end - 1) == '\n' {
        end -= 1;
        if end > start && text.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    end
}

/// A monotonically increasing local text-document version.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentVersion(u64);

impl DocumentVersion {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DocumentVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

static NEXT_TRACKER_ID: AtomicU64 = AtomicU64::new(1);

/// An opaque stamp captured when an asynchronous language-server request is sent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VersionStamp {
    tracker_id: u64,
    version: DocumentVersion,
}

impl VersionStamp {
    pub const fn version(self) -> DocumentVersion {
        self.version
    }
}

/// Per-document monotonic version tracking and stale asynchronous-result rejection.
///
/// Stamps also carry a private tracker identity, so a result for a different document with
/// the same numerical version cannot accidentally pass this gate.
#[derive(Debug)]
pub struct VersionTracker {
    tracker_id: u64,
    current: DocumentVersion,
}

impl Default for VersionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl VersionTracker {
    pub fn new() -> Self {
        Self {
            tracker_id: NEXT_TRACKER_ID.fetch_add(1, Ordering::Relaxed),
            current: DocumentVersion::INITIAL,
        }
    }

    pub fn current(&self) -> DocumentVersion {
        self.current
    }

    /// Advance after every locally applied text change, including undo and redo.
    pub fn advance(&mut self) -> DocumentVersion {
        self.current = DocumentVersion::new(
            self.current
                .get()
                .checked_add(1)
                .expect("document version counter exhausted"),
        );
        self.current
    }

    pub fn stamp(&self) -> VersionStamp {
        VersionStamp {
            tracker_id: self.tracker_id,
            version: self.current,
        }
    }

    pub fn is_current(&self, stamp: VersionStamp) -> bool {
        stamp.tracker_id == self.tracker_id && stamp.version == self.current
    }

    /// Return the result only if no edit has happened since its request was stamped.
    pub fn gate<T>(&self, stamp: VersionStamp, result: T) -> Result<T, StaleResult<T>> {
        if self.is_current(stamp) {
            Ok(result)
        } else {
            Err(StaleResult {
                result,
                requested_at: stamp.version,
                current: self.current,
            })
        }
    }
}

/// A rejected asynchronous result. The value is retained so callers can log or inspect it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StaleResult<T> {
    result: T,
    requested_at: DocumentVersion,
    current: DocumentVersion,
}

impl<T> StaleResult<T> {
    pub fn requested_at(&self) -> DocumentVersion {
        self.requested_at
    }

    pub fn current(&self) -> DocumentVersion {
        self.current
    }

    pub fn result(&self) -> &T {
        &self.result
    }

    pub fn into_result(self) -> T {
        self.result
    }
}

/// Encode one opaque UTF-8 JSON body using LSP's `Content-Length` framing.
pub fn encode_frame(json: &str) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    let mut frame = Vec::with_capacity(header.len() + json.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(json.as_bytes());
    frame
}

/// Write one opaque UTF-8 JSON body using LSP's `Content-Length` framing.
pub fn write_frame(mut writer: impl Write, json: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", json.len())?;
    writer.write_all(json.as_bytes())
}

pub const DEFAULT_MAX_HEADER_BYTES: usize = 8 * 1024;
pub const DEFAULT_MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

/// An incremental decoder for the byte stream read from a language server's stdout.
///
/// Bodies are returned as opaque UTF-8 strings. They are intentionally not checked for JSON
/// syntax, JSON-RPC shape, or method names here. Calls may feed any transport-sized chunk,
/// including chunks that split the header or a multi-byte UTF-8 scalar.
#[derive(Clone, Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_header_bytes: usize,
    max_content_bytes: usize,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_HEADER_BYTES, DEFAULT_MAX_CONTENT_BYTES)
    }

    pub fn with_limits(max_header_bytes: usize, max_content_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_header_bytes,
            max_content_bytes,
        }
    }

    /// Append bytes without attempting to decode them. Use [`Self::next_message`] when a
    /// one-message-at-a-time event loop is more convenient than [`Self::push`].
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Append bytes and return every complete message now available.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, FrameError> {
        self.feed(bytes);
        let mut messages = Vec::new();
        while let Some(message) = self.next_message()? {
            messages.push(message);
        }
        Ok(messages)
    }

    /// Decode at most one message, leaving a partial header or body buffered.
    pub fn next_message(&mut self) -> Result<Option<String>, FrameError> {
        let Some(header_end) = find_header_end(&self.buffer) else {
            // A delimiter may start in the final three bytes, so those bytes do not count
            // against the maximum yet.
            if self.buffer.len() > self.max_header_bytes.saturating_add(3) {
                return Err(FrameError::HeaderTooLarge {
                    max: self.max_header_bytes,
                });
            }
            return Ok(None);
        };

        if header_end > self.max_header_bytes {
            return Err(FrameError::HeaderTooLarge {
                max: self.max_header_bytes,
            });
        }

        let body_len = parse_content_length(&self.buffer[..header_end])?;
        if body_len > self.max_content_bytes {
            return Err(FrameError::ContentTooLarge {
                length: body_len,
                max: self.max_content_bytes,
            });
        }

        let body_start = header_end + 4;
        let frame_end = body_start
            .checked_add(body_len)
            .ok_or(FrameError::ContentLengthOverflow)?;
        if self.buffer.len() < frame_end {
            return Ok(None);
        }

        let body = std::str::from_utf8(&self.buffer[body_start..frame_end]).map_err(|error| {
            FrameError::BodyNotUtf8 {
                valid_up_to: error.valid_up_to(),
            }
        })?;
        let message = body.to_owned();
        self.buffer.drain(..frame_end);
        Ok(Some(message))
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Discard buffered bytes, for example before restarting a server after a protocol error.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameError {
    HeaderTooLarge { max: usize },
    HeaderNotAscii,
    MalformedHeader(String),
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength(String),
    ContentLengthOverflow,
    ContentTooLarge { length: usize, max: usize },
    BodyNotUtf8 { valid_up_to: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooLarge { max } => {
                write!(formatter, "LSP header exceeds the {max}-byte limit")
            }
            Self::HeaderNotAscii => formatter.write_str("LSP header is not ASCII"),
            Self::MalformedHeader(line) => write!(formatter, "malformed LSP header line: {line:?}"),
            Self::MissingContentLength => formatter.write_str("LSP frame has no Content-Length"),
            Self::DuplicateContentLength => {
                formatter.write_str("LSP frame has more than one Content-Length")
            }
            Self::InvalidContentLength(value) => {
                write!(formatter, "invalid LSP Content-Length value: {value:?}")
            }
            Self::ContentLengthOverflow => {
                formatter.write_str("LSP Content-Length overflows this platform")
            }
            Self::ContentTooLarge { length, max } => write!(
                formatter,
                "LSP body length {length} exceeds the {max}-byte limit"
            ),
            Self::BodyNotUtf8 { valid_up_to } => write!(
                formatter,
                "LSP body is not UTF-8 (valid through byte {valid_up_to})"
            ),
        }
    }
}

impl Error for FrameError {}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(header: &[u8]) -> Result<usize, FrameError> {
    if !header.is_ascii() {
        return Err(FrameError::HeaderNotAscii);
    }
    let header = std::str::from_utf8(header).expect("ASCII is valid UTF-8");
    let mut content_length = None;

    for line in header.split("\r\n") {
        let Some((raw_name, raw_value)) = line.split_once(':') else {
            return Err(FrameError::MalformedHeader(line.to_owned()));
        };
        let name = raw_name.trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(FrameError::MalformedHeader(line.to_owned()));
        }

        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(FrameError::DuplicateContentLength);
            }
            let value = raw_value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(FrameError::InvalidContentLength(value.to_owned()));
            }
            let length = value
                .parse::<usize>()
                .map_err(|_| FrameError::ContentLengthOverflow)?;
            content_length = Some(length);
        }
    }

    content_length.ok_or(FrameError::MissingContentLength)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: usize, utf16: usize) -> LspPosition {
        LspPosition::new(Line::new(line), Utf16Offset::new(utf16))
    }

    #[test]
    fn scalar_and_utf16_positions_round_trip_across_unicode_lines() {
        // Combining marks, BMP accents, astral scalars, and a ZWJ emoji sequence all have
        // intentionally different grapheme/scalar/UTF-8/UTF-16 lengths.
        let snapshot = DocumentSnapshot::from_text(
            "a\u{301}🦀 café\n👩\u{200d}💻 𐐷z\nनमस्ते",
            DocumentVersion::new(7),
        );

        let expected = [
            (0, pos(0, 0)),
            (1, pos(0, 1)),
            (2, pos(0, 2)),
            (3, pos(0, 4)),
            (8, pos(0, 9)),
            (9, pos(1, 0)),
            (10, pos(1, 2)),
            (11, pos(1, 3)),
            (12, pos(1, 5)),
            (13, pos(1, 6)),
            (14, pos(1, 8)),
            (15, pos(1, 9)),
            (16, pos(2, 0)),
        ];

        for (scalar, position) in expected {
            let scalar = CharOffset::new(scalar);
            assert_eq!(snapshot.char_to_position(scalar), Ok(position));
            assert_eq!(snapshot.position_to_char(position), Ok(scalar));
        }
    }

    #[test]
    fn utf16_positions_cannot_split_astral_scalars() {
        let snapshot = DocumentSnapshot::from_text("a🦀b", DocumentVersion::INITIAL);
        assert_eq!(
            snapshot.position_to_char(pos(0, 2)),
            Err(CoordinateError::Utf16InsideSurrogatePair {
                line: Line::new(0),
                offset: Utf16Offset::new(2),
            })
        );
        assert_eq!(snapshot.position_to_char(pos(0, 3)), Ok(CharOffset::new(2)));
    }

    #[test]
    fn byte_offsets_must_be_utf8_boundaries() {
        let snapshot = DocumentSnapshot::from_text("é🦀x", DocumentVersion::INITIAL);
        assert_eq!(
            snapshot.char_to_byte(CharOffset::new(0)),
            Ok(ByteOffset::new(0))
        );
        assert_eq!(
            snapshot.char_to_byte(CharOffset::new(1)),
            Ok(ByteOffset::new(2))
        );
        assert_eq!(
            snapshot.char_to_byte(CharOffset::new(2)),
            Ok(ByteOffset::new(6))
        );
        assert_eq!(
            snapshot.byte_to_char(ByteOffset::new(6)),
            Ok(CharOffset::new(2))
        );
        assert_eq!(
            snapshot.byte_to_char(ByteOffset::new(3)),
            Err(CoordinateError::ByteNotCharBoundary {
                offset: ByteOffset::new(3),
            })
        );
    }

    #[test]
    fn line_bounds_and_line_endings_are_strict() {
        let snapshot = DocumentSnapshot::from_text("one\r\n🦀\n", DocumentVersion::INITIAL);
        assert_eq!(snapshot.position_to_char(pos(0, 3)), Ok(CharOffset::new(3)));
        assert_eq!(
            snapshot.char_to_position(CharOffset::new(4)),
            Err(CoordinateError::CursorInsideLineEnding {
                offset: CharOffset::new(4),
            })
        );
        assert_eq!(snapshot.position_to_char(pos(1, 2)), Ok(CharOffset::new(6)));
        assert_eq!(snapshot.char_to_position(CharOffset::new(7)), Ok(pos(2, 0)));
        assert_eq!(
            snapshot.position_to_char(pos(3, 0)),
            Err(CoordinateError::LineOutOfBounds {
                line: Line::new(3),
                line_count: 3,
            })
        );
    }

    #[test]
    fn version_gate_rejects_edits_and_other_documents() {
        let mut tracker = VersionTracker::new();
        let request = tracker.stamp();
        assert_eq!(tracker.gate(request, "diagnostics"), Ok("diagnostics"));

        tracker.advance();
        let stale = tracker.gate(request, "old diagnostics").unwrap_err();
        assert_eq!(stale.requested_at(), DocumentVersion::INITIAL);
        assert_eq!(stale.current(), DocumentVersion::new(1));
        assert_eq!(stale.into_result(), "old diagnostics");

        let other_document = VersionTracker::new();
        let other_stamp = other_document.stamp();
        assert!(!tracker.is_current(other_stamp));
    }

    #[test]
    fn framing_counts_utf8_bytes_and_never_parses_json() {
        let opaque = "{ definitely not JSON: 🦀 }";
        let frame = encode_frame(opaque);
        let header = format!("Content-Length: {}\r\n\r\n", opaque.len());
        assert!(frame.starts_with(header.as_bytes()));

        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&frame).unwrap(), vec![opaque]);
        assert_eq!(decoder.pending_bytes(), 0);
    }

    #[test]
    fn incremental_framing_handles_every_split_inside_emoji() {
        let body = r#"{"text":"a🦀👩‍💻é"}"#;
        let frame = encode_frame(body);

        for split in 0..frame.len() {
            let mut decoder = FrameDecoder::new();
            assert!(decoder.push(&frame[..split]).unwrap().is_empty());
            assert_eq!(decoder.push(&frame[split..]).unwrap(), vec![body]);
        }
    }

    #[test]
    fn framing_decodes_multiple_messages_and_preserves_partial_tail() {
        let first = encode_frame(r#"{"id":1}"#);
        let second = encode_frame(r#"{"id":2,"result":"雪"}"#);
        let third = encode_frame(r#"{"id":3}"#);
        let third_split = third.len() - 2;
        let stream = [first, second, third[..third_split].to_vec()].concat();

        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.push(&stream).unwrap(),
            vec![r#"{"id":1}"#, r#"{"id":2,"result":"雪"}"#]
        );
        assert!(decoder.pending_bytes() > 0);
        assert_eq!(
            decoder.push(&third[third_split..]).unwrap(),
            vec![r#"{"id":3}"#]
        );
    }

    #[test]
    fn framing_accepts_content_type_and_rejects_protocol_errors() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n{}");
        assert_eq!(decoder.next_message().unwrap(), Some("{}".to_owned()));

        let mut missing = FrameDecoder::new();
        missing.feed(b"Content-Type: application/json\r\n\r\n{}");
        assert_eq!(
            missing.next_message(),
            Err(FrameError::MissingContentLength)
        );

        let mut invalid_utf8 = FrameDecoder::new();
        invalid_utf8.feed(b"Content-Length: 2\r\n\r\n\xff\xff");
        assert_eq!(
            invalid_utf8.next_message(),
            Err(FrameError::BodyNotUtf8 { valid_up_to: 0 })
        );
    }

    #[test]
    fn write_frame_matches_vec_encoder() {
        let mut output = Vec::new();
        write_frame(&mut output, "🦀").unwrap();
        assert_eq!(output, encode_frame("🦀"));
    }
}
