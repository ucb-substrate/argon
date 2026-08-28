//! Text-document synchronization for the Argon analyzer.

use arcstr::ArcStr;
use lsp_document::{IndexedText, Pos, TextChange, TextMap, apply_change};
use tower_lsp_server::ls_types::{Position, PositionEncodingKind, Range};

/// The unit an LSP `Position::character` counts.
///
/// LSP defaults to UTF-16 code units. A client that offers UTF-8 is served
/// that instead, because every offset the compiler produces is already a byte
/// offset and the conversion then collapses to the identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PositionEncoding {
    Utf8,
    #[default]
    Utf16,
}

impl PositionEncoding {
    /// Picks an encoding from the list the client advertised in
    /// `initialize.capabilities.general.positionEncodings`.
    ///
    /// The protocol allows the server to answer only with an encoding the
    /// client offered, and requires every client to support UTF-16, so an
    /// absent or unrecognized list falls back to it.
    pub(crate) fn negotiate(offered: Option<&[PositionEncodingKind]>) -> Self {
        match offered {
            Some(kinds) if kinds.contains(&PositionEncodingKind::UTF8) => Self::Utf8,
            _ => Self::Utf16,
        }
    }

    pub(crate) fn kind(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
        }
    }

    /// Width of `c` in the units this encoding counts.
    fn width(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Document {
    contents: IndexedText<ArcStr>,
    version: i32,
    encoding: PositionEncoding,
}

pub(crate) struct DocumentChange {
    pub(crate) range: Option<Range>,
    pub(crate) patch: String,
}

impl Document {
    pub(crate) fn new(
        contents: impl Into<ArcStr>,
        version: i32,
        encoding: PositionEncoding,
    ) -> Self {
        Self {
            contents: IndexedText::new(contents.into()),
            version,
            encoding,
        }
    }

    /// Byte offset of `offset` rendered as a client-facing position.
    ///
    /// Offsets past the end of the text are clamped rather than rejected: they
    /// only arise from spans over generated declarations, and a clamped
    /// position is more useful to the editor than a dropped one.
    pub(crate) fn offset_to_pos(&self, offset: usize) -> Position {
        let text = self.contents.text();
        let mut offset = offset.min(text.len());
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let Some(pos) = self.contents.offset_to_pos(offset) else {
            return Position::new(0, 0);
        };
        let character = match self.encoding {
            PositionEncoding::Utf8 => pos.col,
            PositionEncoding::Utf16 => self
                .contents
                .substr(Pos::new(pos.line, 0)..pos)
                .map(|prefix| prefix.encode_utf16().count() as u32)
                .unwrap_or(pos.col),
        };
        Position::new(pos.line, character)
    }

    /// Byte-offset position of a client-facing position.
    ///
    /// Walking the line rather than indexing it keeps the result on a UTF-8
    /// character boundary even when the client counts UTF-16 units or sends a
    /// character past the end of the line.
    fn position_to_pos(&self, position: Position) -> Option<Pos> {
        let line = self.contents.line_range(position.line)?;
        let target = position.character as usize;
        let text = self.contents.substr(line.start..line.end)?;
        let mut units = 0usize;
        let mut bytes = 0usize;
        for c in text.chars() {
            if units >= target {
                break;
            }
            units += self.encoding.width(c);
            bytes += c.len_utf8();
        }
        Some(Pos::new(position.line, bytes as u32))
    }

    pub(crate) fn position_to_offset(&self, position: Position) -> Option<usize> {
        // A position on or past the virtual line after the final newline is
        // the end of the document; clients legitimately send it.
        let Some(pos) = self.position_to_pos(position) else {
            return Some(self.contents.text().len());
        };
        // `substr` slices the backing text between two line-indexed points, so
        // the length of the prefix ending at `pos` is its byte offset.
        Some(
            self.contents
                .substr(Pos::new(0, 0)..pos)
                .map_or(0, |prefix| prefix.len()),
        )
    }

    pub(crate) fn substr(&self, range: std::ops::Range<Position>) -> Option<&str> {
        let start = self.position_to_offset(range.start)?;
        let end = self.position_to_offset(range.end)?;
        self.contents.text().get(start..end)
    }

    pub(crate) fn apply_changes(&mut self, changes: Vec<DocumentChange>, version: i32) {
        if version > self.version {
            for change in changes {
                let range = match change.range {
                    Some(range) => {
                        let (Some(start), Some(end)) = (
                            self.position_to_pos(range.start),
                            self.position_to_pos(range.end),
                        ) else {
                            continue;
                        };
                        Some(start..end)
                    }
                    None => None,
                };
                self.contents = IndexedText::new(ArcStr::from(apply_change(
                    &self.contents,
                    TextChange {
                        range,
                        patch: change.patch,
                    },
                )));
            }
            self.version = version;
        }
    }

    pub(crate) fn contents(&self) -> &str {
        self.contents.text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `µ` is two UTF-8 bytes and one UTF-16 unit; `𝄞` is four and two.
    const MIXED: &str = "let w = 1.; // µ and 𝄞\nlet h = 2.;\n";

    fn doc(encoding: PositionEncoding) -> Document {
        Document::new(MIXED, 0, encoding)
    }

    #[test]
    fn utf16_positions_count_code_units() {
        let doc = doc(PositionEncoding::Utf16);
        let offset = MIXED.find('𝄞').unwrap();
        // 21 characters precede `𝄞`, one of which (`µ`) is two bytes wide, so
        // the UTF-16 column and the byte offset disagree by one.
        assert_eq!(offset, 22);
        assert_eq!(doc.offset_to_pos(offset), Position::new(0, 21));
        assert_eq!(doc.position_to_offset(Position::new(0, 21)), Some(offset));
    }

    #[test]
    fn utf8_positions_count_bytes() {
        let doc = doc(PositionEncoding::Utf8);
        let offset = MIXED.find('𝄞').unwrap();
        assert_eq!(doc.offset_to_pos(offset), Position::new(0, offset as u32));
        assert_eq!(doc.position_to_offset(Position::new(0, 22)), Some(22));
    }

    #[test]
    fn positions_round_trip_at_every_boundary() {
        for encoding in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            let doc = doc(encoding);
            for (offset, _) in MIXED.char_indices() {
                let position = doc.offset_to_pos(offset);
                assert_eq!(
                    doc.position_to_offset(position),
                    Some(offset),
                    "{encoding:?} at {offset}"
                );
            }
        }
    }

    #[test]
    fn out_of_range_input_is_clamped_rather_than_rejected() {
        let doc = doc(PositionEncoding::Utf16);
        // Past the end of a line clamps to that line's end (which includes its
        // newline), and past the last line clamps to the end of the document.
        let first_line_end = MIXED.find('\n').unwrap() + 1;
        assert_eq!(
            doc.position_to_offset(Position::new(0, 999)),
            Some(first_line_end)
        );
        assert_eq!(
            doc.position_to_offset(Position::new(99, 0)),
            Some(MIXED.len())
        );
        assert_eq!(doc.offset_to_pos(MIXED.len() + 100).line, 1);
    }

    #[test]
    fn an_offset_inside_a_character_does_not_panic() {
        let doc = doc(PositionEncoding::Utf16);
        let offset = MIXED.find('𝄞').unwrap();
        assert_eq!(doc.offset_to_pos(offset + 1), doc.offset_to_pos(offset));
    }

    #[test]
    fn an_empty_document_has_one_position() {
        let doc = Document::new("", 0, PositionEncoding::Utf16);
        assert_eq!(doc.position_to_offset(Position::new(0, 0)), Some(0));
        assert_eq!(doc.offset_to_pos(0), Position::new(0, 0));
    }

    #[test]
    fn incremental_changes_use_the_negotiated_encoding() {
        let mut doc = doc(PositionEncoding::Utf16);
        // Replace `𝄞` (UTF-16 units 21..23 of line 0) with `x`.
        doc.apply_changes(
            vec![DocumentChange {
                range: Some(Range::new(Position::new(0, 21), Position::new(0, 23))),
                patch: "x".to_owned(),
            }],
            1,
        );
        assert_eq!(doc.contents(), "let w = 1.; // µ and x\nlet h = 2.;\n");
    }

    #[test]
    fn changes_older_than_the_current_version_are_ignored() {
        let mut doc = Document::new("abc", 5, PositionEncoding::Utf8);
        doc.apply_changes(
            vec![DocumentChange {
                range: None,
                patch: "xyz".to_owned(),
            }],
            4,
        );
        assert_eq!(doc.contents(), "abc");
    }

    #[test]
    fn utf8_is_negotiated_only_when_offered() {
        assert_eq!(
            PositionEncoding::negotiate(Some(&[
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16
            ])),
            PositionEncoding::Utf8
        );
        assert_eq!(
            PositionEncoding::negotiate(Some(&[PositionEncodingKind::UTF16])),
            PositionEncoding::Utf16
        );
        assert_eq!(PositionEncoding::negotiate(None), PositionEncoding::Utf16);
    }
}
