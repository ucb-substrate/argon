//! Parsing of Neovim's `redraw` notifications.

use rmpv::Value;

use super::ui::CellText;

/// One UI event from a `redraw` notification.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RedrawEvent {
    GridResize {
        width: u16,
        height: u16,
    },
    GridClear,
    GridLine {
        row: u16,
        col: u16,
        cells: Vec<LineCell>,
    },
    GridScroll {
        top: u16,
        bot: u16,
        left: u16,
        right: u16,
        rows: i32,
    },
    GridCursorGoto {
        row: u16,
        col: u16,
    },
    HlAttrDefine {
        id: u32,
        attrs: HlAttrs,
    },
    DefaultColorsSet {
        fg: Option<u32>,
        bg: Option<u32>,
        sp: Option<u32>,
    },
    ModeInfoSet {
        modes: Vec<ModeInfo>,
    },
    ModeChange {
        index: usize,
    },
    OptionSet(UiOption),
    BusyStart,
    BusyStop,
    MouseOn,
    MouseOff,
    Flush,
}

/// A cell of a `grid_line` event, repeated `repeat` times.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LineCell {
    pub text: CellText,
    pub hl: u32,
    pub repeat: u16,
}

/// The RGB attributes of a highlight, as Neovim defines them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HlAttrs {
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub special: Option<u32>,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
    pub undercurl: bool,
}

/// How the cursor is drawn in a mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CursorShape {
    #[default]
    Block,
    Horizontal,
    Vertical,
}

/// The cursor style of one mode from `mode_info_set`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModeInfo {
    pub cursor_shape: CursorShape,
    /// The share of the cell a bar or underline cursor fills, from 1 to 100.
    pub cell_percentage: u8,
    pub attr_id: u32,
}

impl Default for ModeInfo {
    fn default() -> Self {
        Self {
            cursor_shape: CursorShape::Block,
            cell_percentage: 100,
            attr_id: 0,
        }
    }
}

/// A UI option the pane uses.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UiOption {
    Guifont(String),
    Linespace(i64),
}

/// Parses the parameters of a `redraw` notification. Unknown events and
/// events for grids other than 1 are skipped.
pub(crate) fn parse_redraw(params: &[Value]) -> Vec<RedrawEvent> {
    let mut events = Vec::new();
    for entry in params {
        let Some((name, tuples)) = entry.as_array().and_then(|entry| entry.split_first()) else {
            continue;
        };
        let Some(name) = name.as_str() else {
            continue;
        };
        for args in tuples {
            if let Some(event) = args.as_array().and_then(|args| parse_event(name, args)) {
                events.push(event);
            }
        }
    }
    events
}

fn parse_event(name: &str, args: &[Value]) -> Option<RedrawEvent> {
    let on_main_grid = || args.first().and_then(Value::as_u64) == Some(1);
    let arg = |index: usize| args.get(index);
    Some(match name {
        "grid_resize" if on_main_grid() => RedrawEvent::GridResize {
            width: uint(arg(1)?)?,
            height: uint(arg(2)?)?,
        },
        "grid_clear" if on_main_grid() => RedrawEvent::GridClear,
        "grid_line" if on_main_grid() => RedrawEvent::GridLine {
            row: uint(arg(1)?)?,
            col: uint(arg(2)?)?,
            cells: parse_cells(arg(3)?.as_array()?)?,
        },
        "grid_scroll" if on_main_grid() => RedrawEvent::GridScroll {
            top: uint(arg(1)?)?,
            bot: uint(arg(2)?)?,
            left: uint(arg(3)?)?,
            right: uint(arg(4)?)?,
            rows: i32::try_from(arg(5)?.as_i64()?).ok()?,
        },
        "grid_cursor_goto" if on_main_grid() => RedrawEvent::GridCursorGoto {
            row: uint(arg(1)?)?,
            col: uint(arg(2)?)?,
        },
        "hl_attr_define" => RedrawEvent::HlAttrDefine {
            id: uint(arg(0)?)?,
            attrs: parse_hl_attrs(arg(1)?)?,
        },
        "default_colors_set" => RedrawEvent::DefaultColorsSet {
            fg: color(arg(0)?),
            bg: color(arg(1)?),
            sp: color(arg(2)?),
        },
        "mode_info_set" => RedrawEvent::ModeInfoSet {
            modes: arg(1)?.as_array()?.iter().map(parse_mode_info).collect(),
        },
        "mode_change" => RedrawEvent::ModeChange {
            index: uint(arg(1)?)?,
        },
        "option_set" => RedrawEvent::OptionSet(match arg(0)?.as_str()? {
            "guifont" => UiOption::Guifont(arg(1)?.as_str()?.to_owned()),
            "linespace" => UiOption::Linespace(arg(1)?.as_i64()?),
            _ => return None,
        }),
        "busy_start" => RedrawEvent::BusyStart,
        "busy_stop" => RedrawEvent::BusyStop,
        "mouse_on" => RedrawEvent::MouseOn,
        "mouse_off" => RedrawEvent::MouseOff,
        "flush" => RedrawEvent::Flush,
        _ => return None,
    })
}

fn uint<T: TryFrom<u64>>(value: &Value) -> Option<T> {
    value.as_u64().and_then(|value| T::try_from(value).ok())
}

/// An RGB color, or `None` for -1 and other unset values.
fn color(value: &Value) -> Option<u32> {
    value
        .as_i64()
        .and_then(|color| u32::try_from(color).ok())
        .filter(|color| *color <= 0xff_ffff)
}

fn parse_cells(cells: &[Value]) -> Option<Vec<LineCell>> {
    let mut hl = 0;
    cells
        .iter()
        .map(|cell| {
            let cell = cell.as_array()?;
            if let Some(id) = cell.get(1) {
                hl = uint(id)?;
            }
            let repeat = match cell.get(2) {
                Some(repeat) => uint(repeat)?,
                None => 1,
            };
            Some(LineCell {
                text: cell_text(cell.first()?)?,
                hl,
                repeat,
            })
        })
        .collect()
}

fn cell_text(value: &Value) -> Option<CellText> {
    let text = match value {
        Value::String(text) => text
            .as_str()
            .map(Into::into)
            .unwrap_or_else(|| String::from_utf8_lossy(text.as_bytes())),
        _ => return None,
    };
    let mut chars = text.chars();
    Some(match (chars.next(), chars.next()) {
        (None, _) => CellText::Continuation,
        (Some(ch), None) => CellText::Char(ch),
        _ => CellText::Cluster(text.into()),
    })
}

fn parse_hl_attrs(value: &Value) -> Option<HlAttrs> {
    let mut attrs = HlAttrs::default();
    for (key, value) in value.as_map()? {
        let flag = || value.as_bool().unwrap_or(false);
        match key.as_str() {
            Some("foreground") => attrs.foreground = color(value),
            Some("background") => attrs.background = color(value),
            Some("special") => attrs.special = color(value),
            Some("reverse") => attrs.reverse = flag(),
            Some("bold") => attrs.bold = flag(),
            Some("italic") => attrs.italic = flag(),
            Some("strikethrough") => attrs.strikethrough = flag(),
            Some("underline" | "underdouble" | "underdotted" | "underdashed") => {
                attrs.underline |= flag()
            }
            Some("undercurl") => attrs.undercurl = flag(),
            _ => {}
        }
    }
    Some(attrs)
}

fn parse_mode_info(value: &Value) -> ModeInfo {
    let mut mode = ModeInfo::default();
    for (key, value) in value.as_map().map(Vec::as_slice).unwrap_or_default() {
        match key.as_str() {
            Some("cursor_shape") => {
                mode.cursor_shape = match value.as_str() {
                    Some("horizontal") => CursorShape::Horizontal,
                    Some("vertical") => CursorShape::Vertical,
                    _ => CursorShape::Block,
                }
            }
            Some("cell_percentage") => {
                if let Some(percentage) = value.as_u64().filter(|value| (1..=100).contains(value)) {
                    mode.cell_percentage = percentage as u8;
                }
            }
            Some("attr_id") => mode.attr_id = uint(value).unwrap_or(0),
            _ => {}
        }
    }
    mode
}

#[cfg(test)]
mod tests {
    use rmpv::Value;

    use super::*;

    fn event(name: &str, tuples: Vec<Value>) -> Value {
        let mut entry = vec![Value::from(name)];
        entry.extend(tuples);
        Value::Array(entry)
    }

    fn tuple(args: Vec<Value>) -> Value {
        Value::Array(args)
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    #[test]
    fn each_tuple_of_an_event_is_its_own_event() {
        let events = parse_redraw(&[
            event(
                "grid_cursor_goto",
                vec![
                    tuple(vec![1.into(), 2.into(), 3.into()]),
                    tuple(vec![1.into(), 4.into(), 5.into()]),
                ],
            ),
            event("flush", vec![tuple(vec![])]),
        ]);
        assert_eq!(
            events,
            vec![
                RedrawEvent::GridCursorGoto { row: 2, col: 3 },
                RedrawEvent::GridCursorGoto { row: 4, col: 5 },
                RedrawEvent::Flush,
            ]
        );
    }

    #[test]
    fn grid_line_cells_reuse_highlights_repeat_and_mark_wide_characters() {
        let cells = Value::Array(vec![
            tuple(vec!["a".into(), 7.into()]),
            tuple(vec!["b".into()]),
            tuple(vec![" ".into(), 0.into(), 3.into()]),
            tuple(vec!["界".into(), 9.into()]),
            tuple(vec!["".into()]),
            tuple(vec!["e\u{301}".into()]),
        ]);
        let events = parse_redraw(&[event(
            "grid_line",
            vec![tuple(vec![
                1.into(),
                4.into(),
                2.into(),
                cells,
                false.into(),
            ])],
        )]);
        let cell = |text, hl, repeat| LineCell { text, hl, repeat };
        assert_eq!(
            events,
            vec![RedrawEvent::GridLine {
                row: 4,
                col: 2,
                cells: vec![
                    cell(CellText::Char('a'), 7, 1),
                    cell(CellText::Char('b'), 7, 1),
                    cell(CellText::Char(' '), 0, 3),
                    cell(CellText::Char('界'), 9, 1),
                    cell(CellText::Continuation, 9, 1),
                    cell(CellText::Cluster("e\u{301}".into()), 9, 1),
                ],
            }]
        );
    }

    #[test]
    fn unset_colors_parse_to_none() {
        let events = parse_redraw(&[
            event(
                "default_colors_set",
                vec![tuple(vec![
                    0xffffff.into(),
                    (-1).into(),
                    (-1).into(),
                    15.into(),
                    0.into(),
                ])],
            ),
            event(
                "hl_attr_define",
                vec![tuple(vec![
                    3.into(),
                    map(vec![
                        ("foreground", 0x123456.into()),
                        ("background", (-1).into()),
                        ("bold", true.into()),
                        ("underdotted", true.into()),
                        ("undercurl", true.into()),
                    ]),
                    map(vec![]),
                    Value::Array(vec![]),
                ])],
            ),
        ]);
        assert_eq!(
            events,
            vec![
                RedrawEvent::DefaultColorsSet {
                    fg: Some(0xffffff),
                    bg: None,
                    sp: None,
                },
                RedrawEvent::HlAttrDefine {
                    id: 3,
                    attrs: HlAttrs {
                        foreground: Some(0x123456),
                        bold: true,
                        underline: true,
                        undercurl: true,
                        ..HlAttrs::default()
                    },
                },
            ]
        );
    }

    #[test]
    fn unknown_events_and_other_grids_are_skipped() {
        let events = parse_redraw(&[
            event("win_viewport", vec![tuple(vec![1.into()])]),
            event(
                "grid_clear",
                vec![tuple(vec![2.into()]), tuple(vec![1.into()])],
            ),
            event(
                "grid_resize",
                vec![tuple(vec![2.into(), 10.into(), 10.into()])],
            ),
            event(
                "option_set",
                vec![
                    tuple(vec!["ambiwidth".into(), "single".into()]),
                    tuple(vec!["guifont".into(), "Iosevka:h14".into()]),
                    tuple(vec!["linespace".into(), 2.into()]),
                ],
            ),
        ]);
        assert_eq!(
            events,
            vec![
                RedrawEvent::GridClear,
                RedrawEvent::OptionSet(UiOption::Guifont("Iosevka:h14".into())),
                RedrawEvent::OptionSet(UiOption::Linespace(2)),
            ]
        );
    }

    #[test]
    fn modes_keep_their_cursor_shape_size_and_highlight() {
        let events = parse_redraw(&[
            event(
                "mode_info_set",
                vec![tuple(vec![
                    true.into(),
                    Value::Array(vec![
                        map(vec![
                            ("cursor_shape", "block".into()),
                            ("name", "normal".into()),
                        ]),
                        map(vec![
                            ("cursor_shape", "vertical".into()),
                            ("cell_percentage", 25.into()),
                            ("attr_id", 4.into()),
                        ]),
                    ]),
                ])],
            ),
            event("mode_change", vec![tuple(vec!["insert".into(), 1.into()])]),
        ]);
        assert_eq!(
            events,
            vec![
                RedrawEvent::ModeInfoSet {
                    modes: vec![
                        ModeInfo::default(),
                        ModeInfo {
                            cursor_shape: CursorShape::Vertical,
                            cell_percentage: 25,
                            attr_id: 4,
                        },
                    ],
                },
                RedrawEvent::ModeChange { index: 1 },
            ]
        );
    }
}
