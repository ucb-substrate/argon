use arcstr::ArcStr;
use cfgrammar::Span;
use lsp_document::{IndexedText, Pos, TextChange, TextMap, apply_change};
use tower_lsp_server::lsp_types::{Position, Range};

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

    pub(crate) fn span_to_range(&self, span: Span) -> Range {
        Range::new(
            self.offset_to_pos(span.start()),
            self.offset_to_pos(span.end()),
        )
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

    #[allow(dead_code)]
    pub(crate) fn version(&self) -> i32 {
        self.version
    }
}
