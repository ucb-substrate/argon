//! Text-document synchronization for the Argon analyzer.

use arcstr::ArcStr;
use lsp_document::{IndexedText, Pos, TextChange, TextMap, apply_change};
use tower_lsp_server::ls_types::{Position, Range};

#[derive(Debug, Clone)]
pub(crate) struct Document {
    contents: IndexedText<ArcStr>,
    version: i32,
}

pub(crate) struct DocumentChange {
    pub(crate) range: Option<Range>,
    pub(crate) patch: String,
}

fn pos2position(pos: Pos) -> Position {
    Position::new(pos.line, pos.col)
}

fn position2pos(pos: Position) -> Pos {
    Pos {
        line: pos.line,
        col: pos.character,
    }
}

impl Document {
    pub(crate) fn new(contents: impl Into<ArcStr>, version: i32) -> Self {
        Self {
            contents: IndexedText::new(contents.into()),
            version,
        }
    }

    pub(crate) fn offset_to_pos(&self, offset: usize) -> Position {
        pos2position(self.contents.offset_to_pos(offset).unwrap())
    }

    pub(crate) fn pos_to_offset(&self, position: Position) -> Option<usize> {
        let text = self.contents.text();
        let line_start = text
            .split_inclusive('\n')
            .take(position.line as usize)
            .map(str::len)
            .sum::<usize>();
        let line = text.get(line_start..)?.split('\n').next()?;
        let mut utf16_col = 0u32;
        for (byte_col, ch) in line.char_indices() {
            if utf16_col == position.character {
                return Some(line_start + byte_col);
            }
            utf16_col += ch.len_utf16() as u32;
            if utf16_col > position.character {
                return None;
            }
        }
        (utf16_col == position.character).then_some(line_start + line.len())
    }

    pub(crate) fn substr(&self, range: std::ops::Range<Position>) -> &str {
        self.contents
            .substr(position2pos(range.start)..position2pos(range.end))
            .unwrap()
    }

    pub(crate) fn apply_changes(&mut self, changes: Vec<DocumentChange>, version: i32) {
        if version > self.version {
            for change in changes {
                self.contents = IndexedText::new(ArcStr::from(apply_change(
                    &self.contents,
                    TextChange {
                        range: change
                            .range
                            .map(|range| position2pos(range.start)..position2pos(range.end)),
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
    use tower_lsp_server::ls_types::Position;

    use super::Document;

    #[test]
    fn converts_utf16_cursor_positions_to_byte_offsets() {
        let document = Document::new("a😀b\nxy", 0);

        assert_eq!(document.pos_to_offset(Position::new(0, 3)), Some(5));
        assert_eq!(document.pos_to_offset(Position::new(1, 1)), Some(8));
        assert_eq!(document.pos_to_offset(Position::new(0, 2)), None);
    }
}
