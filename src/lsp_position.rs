use crate::{error::SourceRange, line_index::LineIndex};
use tower_lsp_server::ls_types::{Position, Range};

// This trait converts between UTF-8 byte offsets and LSP positions, which are measured in UTF-16
// code units. The reason it's implemented as a trait is so the conversions can use method syntax
// on a line index, which is defined without any LSP types.
pub trait LspPosition {
    fn byte_offset(&self, source_contents: &str, position: Position) -> Option<usize>;
    fn position(&self, source_contents: &str, byte_offset: usize) -> Position;
    fn range(&self, source_contents: &str, source_range: SourceRange) -> Range;
}

impl LspPosition for LineIndex {
    // Convert a zero-based LSP position measured in UTF-16 code units into a UTF-8 byte offset.
    fn byte_offset(&self, source_contents: &str, position: Position) -> Option<usize> {
        // Locate the requested line without counting its line terminator as editor content.
        let line = usize::try_from(position.line).ok()?;
        let line_start = self.line_start(line)?;
        let line_end = self.line_end(source_contents, line)?;
        let content_end = if source_contents.as_bytes().get(line_end) == Some(&b'\n')
            && source_contents[line_start..line_end].ends_with('\r')
        {
            line_end - '\r'.len_utf8()
        } else {
            line_end
        };

        // Reject positions that split a surrogate pair, and clamp positions past the line's
        // contents to its end, as the protocol specifies.
        let requested_character = usize::try_from(position.character).ok()?;
        let mut utf16_character = 0;
        for (index, character) in source_contents[line_start..content_end].char_indices() {
            if utf16_character == requested_character {
                return Some(line_start + index);
            }
            utf16_character += character.len_utf16();
            if utf16_character > requested_character {
                return None;
            }
        }
        Some(content_end)
    }

    // Convert a UTF-8 byte offset into a zero-based LSP position measured in UTF-16 code units.
    fn position(&self, source_contents: &str, byte_offset: usize) -> Position {
        // Find the line containing the offset, which can't extend beyond the source.
        let byte_offset = byte_offset.min(source_contents.len());
        let line = self.line(byte_offset);
        let line_start = self
            .line_start(line)
            .expect("The line containing an offset should exist.");

        // Measure the offset's column within its line. Source ranges originate at character
        // boundaries.
        Position::new(
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(
                source_contents
                    .get(line_start..byte_offset)
                    .expect("Source ranges should end on UTF-8 character boundaries.")
                    .encode_utf16()
                    .count(),
            )
            .unwrap_or(u32::MAX),
        )
    }

    // Convert a source range into the representation expected by the language server protocol.
    fn range(&self, source_contents: &str, source_range: SourceRange) -> Range {
        Range::new(
            self.position(source_contents, source_range.start),
            self.position(source_contents, source_range.end),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LspPosition;
    use crate::line_index::LineIndex;
    use tower_lsp_server::ls_types::Position;

    // Convert a byte offset with a freshly built index.
    fn position(source: &str, byte_offset: usize) -> Position {
        LineIndex::new(source).position(source, byte_offset)
    }

    // Convert an editor position with a freshly built index.
    fn byte_offset(source: &str, position: Position) -> Option<usize> {
        LineIndex::new(source).byte_offset(source, position)
    }

    #[test]
    fn positions_use_utf16_code_units() {
        let source = "zero\n😀 café";

        assert_eq!(position(source, 0), Position::new(0, 0));
        assert_eq!(position(source, 4), Position::new(0, 4));
        assert_eq!(position(source, 5), Position::new(1, 0));
        assert_eq!(position(source, 9), Position::new(1, 2));
        assert_eq!(position(source, source.len()), Position::new(1, 7));
    }

    // Place the offset after a final line break at the start of an empty last line.
    #[test]
    fn positions_after_final_line_break() {
        let source = "zero\n";

        assert_eq!(position(source, source.len()), Position::new(1, 0));
        assert_eq!(byte_offset(source, Position::new(1, 0)), Some(source.len()));
        assert_eq!(byte_offset(source, Position::new(2, 0)), None);
    }

    // Convert editor positions back to byte offsets without splitting Unicode characters, clamping
    // positions past the end of a line to its end.
    #[test]
    fn byte_offsets_use_utf16_code_units() {
        let source = "zero\n😀 café";

        assert_eq!(byte_offset(source, Position::new(0, 0)), Some(0));
        assert_eq!(byte_offset(source, Position::new(0, 9)), Some(4));
        assert_eq!(byte_offset(source, Position::new(1, 0)), Some(5));
        assert_eq!(byte_offset(source, Position::new(1, 1)), None);
        assert_eq!(byte_offset(source, Position::new(1, 2)), Some(9));
        assert_eq!(byte_offset(source, Position::new(1, 7)), Some(source.len()));
        assert_eq!(byte_offset(source, Position::new(1, 8)), Some(source.len()));
        assert_eq!(byte_offset(source, Position::new(2, 0)), None);
        assert_eq!(byte_offset("ab\r\ncd", Position::new(0, 9)), Some(2));
        assert_eq!(byte_offset("ab\r\ncd", Position::new(1, 1)), Some(5));
    }
}
