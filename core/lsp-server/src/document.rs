use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
};

use arcstr::ArcStr;
use compiler::{
    ast::annotated::AnnotatedAst,
    parse::{self, ParseMetadata},
};
use lsp_document::{IndexedText, Pos, TextChange, TextMap, apply_change};
use tower_lsp::lsp_types::{Position, Range, Url};

#[derive(Debug, Clone)]
pub(crate) struct Document {
    contents: IndexedText<String>,
    version: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct GuiDocument {
    pub(crate) doc: Document,
    pub(crate) ast: AnnotatedAst<ParseMetadata>,
    pub(crate) cell: String,
}

impl Deref for GuiDocument {
    type Target = Document;

    fn deref(&self) -> &Self::Target {
        &self.doc
    }
}

impl DerefMut for GuiDocument {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.doc
    }
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
    pub(crate) fn new(contents: impl Into<String>, version: i32) -> Self {
        Self {
            contents: IndexedText::new(contents.into()),
            version,
        }
    }

    pub(crate) fn offset_to_pos(&self, offset: usize) -> Position {
        pos2position(self.contents.offset_to_pos(offset).unwrap())
    }

    #[allow(dead_code)]
    pub(crate) fn substr(&self, range: std::ops::Range<Position>) -> &str {
        self.contents
            .substr(position2pos(range.start)..position2pos(range.end))
            .unwrap()
    }

    pub(crate) fn apply_changes(&mut self, changes: Vec<DocumentChange>, version: i32) {
        if version > self.version {
            for change in changes {
                self.contents = IndexedText::new(apply_change(
                    &self.contents,
                    TextChange {
                        range: change
                            .range
                            .map(|range| position2pos(range.start)..position2pos(range.end)),
                        patch: change.patch,
                    },
                ));
            }
            self.version = version;
        }
    }

    pub(crate) fn contents(&self) -> &str {
        self.contents.text()
    }

    pub(crate) fn version(&self) -> i32 {
        self.version
    }
}

impl GuiDocument {
    pub(crate) fn apply_changes(&mut self, changes: Vec<DocumentChange>, version: i32) {
        if version > self.version() {
            self.doc.apply_changes(changes, version);
            self.ast = AnnotatedAst::new(
                ArcStr::from(self.contents()),
                &parse::parse(self.contents()).unwrap(),
            );
        }
    }
}
