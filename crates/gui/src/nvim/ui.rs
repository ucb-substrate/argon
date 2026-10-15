//! The state of Neovim's screen, built from redraw events.

use std::collections::HashMap;

use gpui::{FontStyle, FontWeight, Hsla, rgb};

use super::redraw::{HlAttrs, ModeInfo, RedrawEvent, UiOption};

/// Colors used until Neovim sets its own defaults.
const FALLBACK_FG: u32 = 0xffffff;
const FALLBACK_BG: u32 = 0x000000;

/// The text of one grid cell.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellText {
    Char(char),
    /// A grapheme cluster longer than one `char`.
    Cluster(Box<str>),
    /// The right half of a double-width character.
    Continuation,
}

/// One cell of the Neovim grid.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cell {
    pub text: CellText,
    pub hl: u32,
}

impl Cell {
    const BLANK: Cell = Cell {
        text: CellText::Char(' '),
        hl: 0,
    };
}

/// A highlight with reverse and default colors applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Highlight {
    pub fg: Hsla,
    pub bg: Hsla,
    pub sp: Hsla,
    pub weight: FontWeight,
    pub style: FontStyle,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
}

/// The default colors from `default_colors_set`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DefaultColors {
    fg: Option<u32>,
    bg: Option<u32>,
    sp: Option<u32>,
}

/// Neovim's main grid and the UI state that goes with it.
#[derive(Clone, Debug)]
pub(crate) struct UiState {
    pub rows: Vec<Vec<Cell>>,
    pub cursor: (u16, u16),
    pub modes: Vec<ModeInfo>,
    pub mode_index: usize,
    pub busy: bool,
    pub mouse_enabled: bool,
    pub guifont: Option<String>,
    pub linespace: i64,
    defaults: DefaultColors,
    attrs: HashMap<u32, HlAttrs>,
    /// Resolved highlights by id; index 0 is the default colors.
    highlights: Vec<Highlight>,
}

impl Default for UiState {
    fn default() -> Self {
        let defaults = DefaultColors::default();
        Self {
            rows: Vec::new(),
            cursor: (0, 0),
            modes: Vec::new(),
            mode_index: 0,
            busy: false,
            mouse_enabled: false,
            guifont: None,
            linespace: 0,
            defaults,
            attrs: HashMap::new(),
            highlights: vec![resolve(&HlAttrs::default(), defaults)],
        }
    }
}

impl UiState {
    pub fn width(&self) -> usize {
        self.rows.first().map_or(0, Vec::len)
    }

    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// The resolved highlight `id`, or the default colors if it is undefined.
    pub fn highlight(&self, id: u32) -> &Highlight {
        self.highlights
            .get(id as usize)
            .unwrap_or(&self.highlights[0])
    }

    /// The cursor style of the current mode.
    pub fn mode(&self) -> ModeInfo {
        self.modes.get(self.mode_index).cloned().unwrap_or_default()
    }

    /// The text of `row`, with continuation cells left out.
    #[cfg(test)]
    pub fn row_text(&self, row: usize) -> String {
        self.rows[row]
            .iter()
            .map(|cell| match &cell.text {
                CellText::Char(ch) => ch.to_string(),
                CellText::Cluster(text) => text.to_string(),
                CellText::Continuation => String::new(),
            })
            .collect()
    }

    pub fn apply(&mut self, event: RedrawEvent) {
        match event {
            RedrawEvent::GridResize { width, height } => self.resize(width, height),
            RedrawEvent::GridClear => {
                for row in &mut self.rows {
                    row.fill(Cell::BLANK);
                }
            }
            RedrawEvent::GridLine { row, col, cells } => {
                let Some(row) = self.rows.get_mut(row as usize) else {
                    return;
                };
                let mut col = col as usize;
                for cell in cells {
                    for _ in 0..cell.repeat {
                        let Some(target) = row.get_mut(col) else {
                            return;
                        };
                        *target = Cell {
                            text: cell.text.clone(),
                            hl: cell.hl,
                        };
                        col += 1;
                    }
                }
            }
            RedrawEvent::GridScroll {
                top,
                bot,
                left,
                right,
                rows,
            } => self.scroll(top, bot, left, right, rows),
            RedrawEvent::GridCursorGoto { row, col } => self.cursor = (row, col),
            RedrawEvent::HlAttrDefine { id, attrs } => {
                if id == 0 {
                    return;
                }
                let index = id as usize;
                if self.highlights.len() <= index {
                    self.highlights.resize(index + 1, self.highlights[0]);
                }
                self.highlights[index] = resolve(&attrs, self.defaults);
                self.attrs.insert(id, attrs);
            }
            RedrawEvent::DefaultColorsSet { fg, bg, sp } => {
                self.defaults = DefaultColors { fg, bg, sp };
                let default = resolve(&HlAttrs::default(), self.defaults);
                self.highlights.fill(default);
                for (id, attrs) in &self.attrs {
                    self.highlights[*id as usize] = resolve(attrs, self.defaults);
                }
            }
            RedrawEvent::ModeInfoSet { modes } => self.modes = modes,
            RedrawEvent::ModeChange { index } => self.mode_index = index,
            RedrawEvent::OptionSet(UiOption::Guifont(font)) => {
                self.guifont = Some(font).filter(|font| !font.is_empty())
            }
            RedrawEvent::OptionSet(UiOption::Linespace(linespace)) => self.linespace = linespace,
            RedrawEvent::BusyStart => self.busy = true,
            RedrawEvent::BusyStop => self.busy = false,
            RedrawEvent::MouseOn => self.mouse_enabled = true,
            RedrawEvent::MouseOff => self.mouse_enabled = false,
            RedrawEvent::Flush => {}
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = (width as usize, height as usize);
        self.rows.resize_with(height, || vec![Cell::BLANK; width]);
        for row in &mut self.rows {
            row.resize(width, Cell::BLANK);
        }
        self.cursor = (
            self.cursor.0.min(height.saturating_sub(1) as u16),
            self.cursor.1.min(width.saturating_sub(1) as u16),
        );
    }

    /// Moves the region `top..bot` × `left..right` by `rows`, upward when
    /// positive. Vacated rows keep stale cells until Neovim redraws them.
    fn scroll(&mut self, top: u16, bot: u16, left: u16, right: u16, rows: i32) {
        let width = self.width();
        let bot = (bot as usize).min(self.rows.len());
        let top = (top as usize).min(bot);
        let right = (right as usize).min(width);
        let left = (left as usize).min(right);
        let count = (rows.unsigned_abs() as usize).min(bot - top);
        if count == 0 {
            return;
        }
        if left == 0 && right == width {
            if rows > 0 {
                self.rows[top..bot].rotate_left(count);
            } else {
                self.rows[top..bot].rotate_right(count);
            }
        } else if rows > 0 {
            for row in top..bot - count {
                let (to, from) = self.rows.split_at_mut(row + count);
                to[row][left..right].clone_from_slice(&from[0][left..right]);
            }
        } else {
            for row in (top + count..bot).rev() {
                let (from, to) = self.rows.split_at_mut(row);
                to[0][left..right].clone_from_slice(&from[row - count][left..right]);
            }
        }
    }
}

fn resolve(attrs: &HlAttrs, defaults: DefaultColors) -> Highlight {
    let mut fg = attrs.foreground.or(defaults.fg).unwrap_or(FALLBACK_FG);
    let mut bg = attrs.background.or(defaults.bg).unwrap_or(FALLBACK_BG);
    let sp = attrs.special.or(defaults.sp).unwrap_or(fg);
    if attrs.reverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    Highlight {
        fg: rgb(fg).into(),
        bg: rgb(bg).into(),
        sp: rgb(sp).into(),
        weight: if attrs.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if attrs.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
        underline: attrs.underline,
        undercurl: attrs.undercurl,
        strikethrough: attrs.strikethrough,
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Hsla, rgb};

    use super::*;
    use crate::nvim::redraw::LineCell;

    fn line(row: u16, col: u16, text: &str) -> RedrawEvent {
        RedrawEvent::GridLine {
            row,
            col,
            cells: text
                .chars()
                .map(|ch| LineCell {
                    text: CellText::Char(ch),
                    hl: 0,
                    repeat: 1,
                })
                .collect(),
        }
    }

    /// A 4 × 4 grid whose rows read `aaaa`, `bbbb`, `cccc`, and `dddd`.
    fn lettered() -> UiState {
        let mut ui = UiState::default();
        ui.apply(RedrawEvent::GridResize {
            width: 4,
            height: 4,
        });
        for (row, text) in ["aaaa", "bbbb", "cccc", "dddd"].into_iter().enumerate() {
            ui.apply(line(row as u16, 0, text));
        }
        ui
    }

    fn rows(ui: &UiState) -> Vec<String> {
        (0..ui.height()).map(|row| ui.row_text(row)).collect()
    }

    fn color(value: u32) -> Hsla {
        rgb(value).into()
    }

    #[test]
    fn grid_lines_repeat_cells_and_keep_wide_characters() {
        let mut ui = UiState::default();
        ui.apply(RedrawEvent::GridResize {
            width: 6,
            height: 1,
        });
        ui.apply(RedrawEvent::GridLine {
            row: 0,
            col: 0,
            cells: vec![
                LineCell {
                    text: CellText::Char('-'),
                    hl: 2,
                    repeat: 3,
                },
                LineCell {
                    text: CellText::Char('界'),
                    hl: 2,
                    repeat: 1,
                },
                LineCell {
                    text: CellText::Continuation,
                    hl: 2,
                    repeat: 1,
                },
                // Cells past the edge are dropped.
                LineCell {
                    text: CellText::Char('x'),
                    hl: 0,
                    repeat: 4,
                },
            ],
        });
        assert_eq!(ui.row_text(0), "---界x");
        assert_eq!(ui.rows[0][4].text, CellText::Continuation);
        assert_eq!(ui.rows[0][0].hl, 2);
    }

    #[test]
    fn full_width_scrolls_move_rows_in_both_directions() {
        let mut ui = lettered();
        ui.apply(RedrawEvent::GridScroll {
            top: 0,
            bot: 4,
            left: 0,
            right: 4,
            rows: 1,
        });
        assert_eq!(rows(&ui)[..3], ["bbbb", "cccc", "dddd"]);

        let mut ui = lettered();
        ui.apply(RedrawEvent::GridScroll {
            top: 1,
            bot: 4,
            left: 0,
            right: 4,
            rows: -2,
        });
        assert_eq!(rows(&ui)[0], "aaaa");
        assert_eq!(rows(&ui)[3], "bbbb");
    }

    #[test]
    fn partial_width_scrolls_move_only_their_columns() {
        let mut ui = lettered();
        ui.apply(RedrawEvent::GridScroll {
            top: 0,
            bot: 3,
            left: 1,
            right: 3,
            rows: 1,
        });
        assert_eq!(rows(&ui)[..2], ["abba", "bccb"]);
        assert_eq!(rows(&ui)[3], "dddd");

        let mut ui = lettered();
        ui.apply(RedrawEvent::GridScroll {
            top: 0,
            bot: 4,
            left: 2,
            right: 4,
            rows: -1,
        });
        assert_eq!(rows(&ui)[1..], ["bbaa", "ccbb", "ddcc"]);
    }

    #[test]
    fn resizing_keeps_the_top_left_and_clearing_blanks_every_cell() {
        let mut ui = lettered();
        ui.cursor = (3, 3);
        ui.apply(RedrawEvent::GridResize {
            width: 2,
            height: 5,
        });
        assert_eq!(rows(&ui), ["aa", "bb", "cc", "dd", "  "]);
        assert_eq!(ui.cursor, (3, 1));

        ui.apply(RedrawEvent::GridClear);
        assert!(ui.rows.iter().flatten().all(|cell| *cell == Cell::BLANK));
    }

    #[test]
    fn highlights_resolve_reverse_and_follow_default_colors() {
        let mut ui = UiState::default();
        ui.apply(RedrawEvent::HlAttrDefine {
            id: 2,
            attrs: HlAttrs {
                foreground: Some(0x112233),
                reverse: true,
                bold: true,
                ..HlAttrs::default()
            },
        });
        ui.apply(RedrawEvent::DefaultColorsSet {
            fg: Some(0xeeeeee),
            bg: Some(0x101010),
            sp: None,
        });
        let reversed = ui.highlight(2);
        assert_eq!(reversed.fg, color(0x101010));
        assert_eq!(reversed.bg, color(0x112233));
        assert_eq!(reversed.weight, FontWeight::BOLD);

        let default = ui.highlight(0);
        assert_eq!(default.fg, color(0xeeeeee));
        assert_eq!(default.bg, color(0x101010));
        assert_eq!(ui.highlight(99), default, "undefined ids use the defaults");

        ui.apply(RedrawEvent::DefaultColorsSet {
            fg: Some(0x000000),
            bg: Some(0xffffff),
            sp: Some(0xff0000),
        });
        assert_eq!(ui.highlight(2).fg, color(0xffffff));
        assert_eq!(ui.highlight(2).sp, color(0xff0000));
    }
}
