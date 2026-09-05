//! Resolving LSP positions to byte offsets in a document.

use lsp_types::{Position, Range};

use crate::bridge::encoding::EncodingConverter;
use crate::error::{Error, Result};

/// Byte offsets of each line start in a document, so an LSP position can be
/// resolved without rescanning the text.
///
/// Lines are split on `\n` only. A `\r` stays part of the line it terminates,
/// matching how a server counts columns in a CRLF file, and a document ending
/// in a newline gets one final empty line, which LSP treats as addressable.
pub struct LineTable<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineTable<'a> {
    /// Index `text` by line start.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { text, line_starts }
    }

    /// The text of one line, without its terminating `\n`.
    fn line(&self, line: u32) -> Result<&'a str> {
        let index = line as usize;
        let start = *self.line_starts.get(index).ok_or_else(|| {
            Error::ApplyRefused(format!(
                "line {line} is beyond the document, which has {} lines",
                self.line_starts.len()
            ))
        })?;
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.text.len(), |next| next - 1);
        Ok(&self.text[start..end])
    }

    /// Byte offset of `position`, with columns counted in `converter`'s
    /// encoding.
    ///
    /// A column past the end of its line resolves to the line's end, which
    /// the LSP specification requires. A line past the end of the document
    /// is an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when the line is beyond the document
    /// or the column does not land on a character boundary.
    pub fn byte_offset(&self, position: Position, converter: &EncodingConverter) -> Result<usize> {
        let line_text = self.line(position.line)?;
        let line_length = converter
            .byte_offset_to_character(line_text, line_text.len())
            .map_err(|e| Error::ApplyRefused(format!("measuring line {}: {e}", position.line)))?;
        let character = position.character.min(line_length);
        let within_line = converter
            .character_to_byte_offset(line_text, character)
            .map_err(|e| {
                Error::ApplyRefused(format!(
                    "column {} on line {}: {e}",
                    position.character, position.line
                ))
            })?;
        Ok(self.line_starts[position.line as usize] + within_line)
    }

    /// Byte range of `range`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ApplyRefused`] when either endpoint fails to resolve
    /// or the end precedes the start.
    pub fn byte_range(
        &self,
        range: Range,
        converter: &EncodingConverter,
    ) -> Result<std::ops::Range<usize>> {
        let start = self.byte_offset(range.start, converter)?;
        let end = self.byte_offset(range.end, converter)?;
        if end < start {
            return Err(Error::ApplyRefused(format!(
                "range end {end} precedes start {start}"
            )));
        }
        Ok(start..end)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use lsp_types::{Position, Range};

    use super::LineTable;
    use crate::bridge::encoding::{EncodingConverter, PositionEncoding};

    fn utf16() -> EncodingConverter {
        EncodingConverter::new(PositionEncoding::Utf16)
    }

    #[test]
    fn test_resolves_ascii_position() {
        let table = LineTable::new("fn main() {}\nlet x = 1;\n");
        let offset = table
            .byte_offset(Position::new(1, 4), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 17, "line 1 starts at 13, plus 4 columns");
    }

    #[test]
    fn test_counts_utf16_units_not_bytes() {
        // "héllo" is 6 bytes and 5 UTF-16 units; column 3 sits after "hél".
        let table = LineTable::new("héllo\n");
        let offset = table
            .byte_offset(Position::new(0, 3), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 4, "h=1, é=2, l=1");
    }

    #[test]
    fn test_counts_utf8_bytes_when_the_server_negotiated_utf8() {
        let table = LineTable::new("héllo\n");
        let converter = EncodingConverter::new(PositionEncoding::Utf8);
        let offset = table
            .byte_offset(Position::new(0, 4), &converter)
            .expect("position resolves");
        assert_eq!(offset, 4, "in UTF-8 the column is already a byte offset");
    }

    #[test]
    fn test_crlf_line_keeps_carriage_return() {
        let table = LineTable::new("ab\r\ncd\r\n");
        let offset = table
            .byte_offset(Position::new(1, 2), &utf16())
            .expect("position resolves");
        assert_eq!(offset, 6, "line 1 starts at 4 and \\r belongs to line 0");
    }

    #[test]
    fn test_position_on_line_after_final_terminator() {
        let table = LineTable::new("a\n");
        let offset = table
            .byte_offset(Position::new(1, 0), &utf16())
            .expect("the empty final line is addressable");
        assert_eq!(offset, 2);
    }

    #[test]
    fn test_column_past_end_of_line_clamps_to_the_line_end() {
        let table = LineTable::new("abc\ndef\n");
        let offset = table
            .byte_offset(Position::new(0, 99), &utf16())
            .expect("the spec says clamp, not refuse");
        assert_eq!(offset, 3, "end of line 0, before its newline");
    }

    #[test]
    fn test_line_beyond_document_is_an_error() {
        let table = LineTable::new("a\n");
        assert!(table.byte_offset(Position::new(9, 0), &utf16()).is_err());
    }

    #[test]
    fn test_byte_range_spans_lines() {
        let table = LineTable::new("abc\ndef\n");
        let range = table
            .byte_range(
                Range::new(Position::new(0, 1), Position::new(1, 2)),
                &utf16(),
            )
            .expect("range resolves");
        assert_eq!(range, 1..6);
    }

    #[test]
    fn test_inverted_range_is_an_error() {
        let table = LineTable::new("abc\ndef\n");
        let range = Range::new(Position::new(1, 2), Position::new(0, 1));
        assert!(table.byte_range(range, &utf16()).is_err());
    }
}
