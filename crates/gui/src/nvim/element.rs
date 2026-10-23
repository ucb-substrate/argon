//! The element that paints the Neovim grid.

use std::ops::Range;

use gpui::{
    App, BorderStyle, Bounds, ContentMask, DispatchPhase, Edges, Element, ElementId,
    ElementInputHandler, Entity, Font, GlobalElementId, Hitbox, HitboxBehavior, Hsla, InputHandler,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, ScrollEvent, ShapedLine, SharedString, Size, StrikethroughStyle,
    Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, fill, font, outline, point,
    px, relative, size,
};

use super::{
    GridLayout, NvimPane,
    redraw::CursorShape,
    ui::{Cell, CellText, Highlight, UiState},
};
use crate::assets::MONO_FAMILY;

/// The font size used when `guifont` gives none.
const DEFAULT_FONT_SIZE: f32 = 13.;

/// The font family and size the grid is drawn with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FontChoice {
    pub family: SharedString,
    pub size: Pixels,
}

/// Monospace families tried, in order, when no `guifont` entry resolves.
const DEFAULT_FAMILIES: &[&str] = &[
    MONO_FAMILY,
    "Menlo",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
];

/// The fonts `guifont` lists, each with its size if it gives one. A
/// backslash escapes the next character.
pub(crate) fn parse_guifont(guifont: &str) -> Vec<(String, Option<f32>)> {
    let mut fonts = Vec::new();
    let mut chars = guifont.chars();
    loop {
        let mut name = String::new();
        let mut options = String::new();
        let mut in_options = false;
        let mut ended = true;
        while let Some(ch) = chars.next() {
            let ch = match ch {
                '\\' => match chars.next() {
                    Some(ch) => ch,
                    None => break,
                },
                ',' => {
                    ended = false;
                    break;
                }
                ':' if !in_options => {
                    in_options = true;
                    continue;
                }
                ch => ch,
            };
            if in_options {
                options.push(ch);
            } else {
                name.push(ch);
            }
        }
        let name = name.trim();
        if !name.is_empty() && name != "*" {
            let size = options
                .split(':')
                .find_map(|option| option.strip_prefix('h')?.parse::<f32>().ok())
                .filter(|size| size.is_finite() && *size > 0.);
            fonts.push((name.to_owned(), size));
        }
        if ended {
            return fonts;
        }
    }
}

/// The first font in `guifont` that `resolves`, or else the first default
/// family that does, at 13 px.
fn choose_font(guifont: Option<&str>, resolves: impl Fn(&str) -> bool) -> FontChoice {
    let requested = guifont.map(parse_guifont).unwrap_or_default();
    if let Some((family, size)) = requested.iter().find(|(family, _)| resolves(family)) {
        return FontChoice {
            family: family.clone().into(),
            size: px(size.unwrap_or(DEFAULT_FONT_SIZE)),
        };
    }
    if let Some(guifont) = guifont {
        tracing::warn!("no font in guifont `{guifont}` is installed");
    }
    let family = DEFAULT_FAMILIES
        .iter()
        .copied()
        .find(|family| resolves(family))
        .unwrap_or(MONO_FAMILY);
    FontChoice {
        family: family.into(),
        size: px(DEFAULT_FONT_SIZE),
    }
}

/// Rounds `value` to the nearest device pixel.
fn snap(value: Pixels, scale: f32) -> Pixels {
    px((f32::from(value) * scale).round() / scale)
}

/// The size of a cell and what text in it needs.
struct Metrics {
    font: FontChoice,
    cell: Size<Pixels>,
}

impl NvimPane {
    fn metrics(&mut self, window: &Window) -> Metrics {
        let guifont = self.ui.guifont.clone();
        let choice = match &self.font {
            Some((resolved_for, choice)) if *resolved_for == guifont => choice.clone(),
            _ => {
                let text_system = window.text_system();
                // GPUI substitutes another family for a font it cannot load.
                let resolves = |family: &str| {
                    let font_id = text_system.resolve_font(&font(family.to_owned()));
                    text_system
                        .get_font_for_id(font_id)
                        .is_some_and(|resolved| resolved.family == family)
                };
                let choice = choose_font(guifont.as_deref(), resolves);
                self.font = Some((guifont, choice.clone()));
                choice
            }
        };
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font(choice.family.clone()));
        let width = text_system
            .advance(font_id, choice.size, 'M')
            .map(|advance| advance.width)
            .unwrap_or(choice.size * 0.6);
        let height = text_system.ascent(font_id, choice.size)
            + text_system.descent(font_id, choice.size).abs()
            + px(self.ui.linespace as f32);
        let scale = window.scale_factor();
        let height = px((f32::from(height) * scale).ceil().max(1.) / scale);
        Metrics {
            font: choice,
            cell: size(width.max(px(1.)), height),
        }
    }
}

fn styled_font(family: &SharedString, highlight: &Highlight) -> Font {
    Font {
        weight: highlight.weight,
        style: highlight.style,
        ..font(family.clone())
    }
}

/// A text run of `highlight`, colored `color`.
fn text_run(text: &str, family: &SharedString, highlight: &Highlight, color: Hsla) -> TextRun {
    TextRun {
        len: text.len(),
        font: styled_font(family, highlight),
        color,
        background_color: None,
        underline: (highlight.underline || highlight.undercurl).then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(highlight.sp),
            wavy: highlight.undercurl,
        }),
        strikethrough: highlight.strikethrough.then(|| StrikethroughStyle {
            thickness: px(1.),
            color: Some(color),
        }),
    }
}

/// A span of one row that shapes as a single line.
struct Run {
    col: usize,
    text: String,
    hl: u32,
}

/// Splits `row` into runs: spans of ASCII cells that share a highlight, and
/// single cells for everything else.
fn row_runs(row: &[Cell]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    // Whether the last run is ASCII and ends at the previous cell.
    let mut extendable = false;
    for (col, cell) in row.iter().enumerate() {
        let text = match &cell.text {
            CellText::Continuation => {
                extendable = false;
                continue;
            }
            CellText::Char(ch) if ch.is_ascii() => {
                if extendable
                    && let Some(run) = runs.last_mut()
                    && run.hl == cell.hl
                {
                    run.text.push(*ch);
                    continue;
                }
                extendable = true;
                ch.to_string()
            }
            CellText::Char(ch) => {
                extendable = false;
                ch.to_string()
            }
            CellText::Cluster(text) => {
                extendable = false;
                text.to_string()
            }
        };
        runs.push(Run {
            col,
            text,
            hl: cell.hl,
        });
    }
    runs
}

/// The pane's input handler, with macOS press-and-hold turned off so that
/// held keys repeat instead of opening the accent menu.
struct PaneInputHandler(ElementInputHandler<NvimPane>);

impl InputHandler for PaneInputHandler {
    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.0
            .selected_text_range(ignore_disabled_input, window, cx)
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.0.marked_text_range(window, cx)
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.0
            .text_for_range(range_utf16, adjusted_range, window, cx)
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0
            .replace_text_in_range(replacement_range, text, window, cx)
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0
            .replace_and_mark_text_in_range(range_utf16, new_text, new_selected_range, window, cx)
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.0.unmark_text(window, cx)
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.0.bounds_for_range(range_utf16, window, cx)
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.0.character_index_for_point(point, window, cx)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
        self.0.accepts_text_input(window, cx)
    }
}

pub(crate) struct NvimElement {
    pane: Entity<NvimPane>,
}

impl NvimElement {
    pub(crate) fn new(pane: Entity<NvimPane>) -> Self {
        Self { pane }
    }
}

/// Quads and text that paint together, quads first.
#[derive(Default)]
struct Layer {
    quads: Vec<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
}

impl Layer {
    fn paint(&mut self, line_height: Pixels, window: &mut Window, cx: &mut App) {
        for quad in self.quads.drain(..) {
            window.paint_quad(quad);
        }
        for (origin, line) in self.lines.drain(..) {
            let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
        }
    }
}

pub(crate) struct PrepaintState {
    hitbox: Hitbox,
    background: Hsla,
    /// The grid's backgrounds and text.
    grid: Layer,
    /// The cursor, or the IME's marked text, over the grid.
    cursor: Layer,
    line_height: Pixels,
}

impl IntoElement for NvimElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Builds the background quads and text lines for `ui`.
struct Painter<'a> {
    ui: &'a UiState,
    window: &'a Window,
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    font: &'a FontChoice,
    scale: f32,
    grid: Layer,
    cursor: Layer,
}

impl Painter<'_> {
    fn cell_origin(&self, row: usize, col: usize) -> Point<Pixels> {
        point(
            self.origin.x + self.cell.width * col as f32,
            self.origin.y + self.cell.height * row as f32,
        )
    }

    /// A quad from `cols` of `row`, with edges on device pixels.
    fn span_bounds(&self, row: usize, cols: std::ops::Range<usize>) -> Bounds<Pixels> {
        let top_left = self.cell_origin(row, cols.start);
        let bottom_right = self.cell_origin(row + 1, cols.end);
        Bounds::from_corners(
            point(snap(top_left.x, self.scale), snap(top_left.y, self.scale)),
            point(
                snap(bottom_right.x, self.scale),
                snap(bottom_right.y, self.scale),
            ),
        )
    }

    fn shape(&self, text: &str, highlight: &Highlight, color: Hsla) -> ShapedLine {
        self.window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            self.font.size,
            &[text_run(text, &self.font.family, highlight, color)],
            None,
        )
    }

    fn backgrounds(&mut self, default: Hsla) {
        for (row_index, row) in self.ui.rows.iter().enumerate() {
            let mut start = 0;
            while start < row.len() {
                let background = self.ui.highlight(row[start].hl).bg;
                let end = row[start..]
                    .iter()
                    .position(|cell| self.ui.highlight(cell.hl).bg != background)
                    .map_or(row.len(), |offset| start + offset);
                if background != default {
                    let bounds = self.span_bounds(row_index, start..end);
                    self.grid.quads.push(fill(bounds, background));
                }
                start = end;
            }
        }
    }

    fn text(&mut self) {
        for (row_index, row) in self.ui.rows.iter().enumerate() {
            for run in row_runs(row) {
                let highlight = self.ui.highlight(run.hl);
                let decorated =
                    highlight.underline || highlight.undercurl || highlight.strikethrough;
                if !decorated && run.text.chars().all(|ch| ch == ' ') {
                    continue;
                }
                let line = self.shape(&run.text, highlight, highlight.fg);
                self.grid
                    .lines
                    .push((self.cell_origin(row_index, run.col), line));
            }
        }
    }

    fn cursor(&mut self, focused: bool, marked_text: Option<&str>) {
        let (row, col) = (self.ui.cursor.0 as usize, self.ui.cursor.1 as usize);
        let Some(cells) = self.ui.rows.get(row) else {
            return;
        };
        let Some(cell) = cells.get(col) else {
            return;
        };
        let origin = self.cell_origin(row, col);
        if let Some(text) = marked_text {
            let default = *self.ui.highlight(0);
            let marked = Highlight {
                underline: true,
                sp: default.fg,
                ..default
            };
            let line = self.shape(text, &marked, default.fg);
            let bounds = Bounds::new(origin, size(line.width, self.cell.height));
            self.cursor.quads.push(fill(bounds, default.bg));
            self.cursor.lines.push((origin, line));
            return;
        }
        if self.ui.busy {
            return;
        }
        let mode = self.ui.mode();
        let under = *self.ui.highlight(cell.hl);
        let (color, text_color) = if mode.attr_id == 0 {
            (under.fg, under.bg)
        } else {
            let cursor = self.ui.highlight(mode.attr_id);
            (cursor.bg, cursor.fg)
        };
        let wide = matches!(
            cells.get(col + 1).map(|cell| &cell.text),
            Some(CellText::Continuation)
        );
        let width = self.cell.width * if wide { 2. } else { 1. };
        let block = Bounds::new(origin, size(width, self.cell.height));
        if !focused {
            self.cursor
                .quads
                .push(outline(block, color, Edges::all(BorderStyle::Solid)));
            return;
        }
        let device_pixel = px(1. / self.scale);
        match mode.cursor_shape {
            CursorShape::Block => {
                self.cursor.quads.push(fill(block, color));
                let text = match &cell.text {
                    CellText::Char(ch) => ch.to_string(),
                    CellText::Cluster(text) => text.to_string(),
                    CellText::Continuation => String::new(),
                };
                if !text.trim().is_empty() {
                    let line = self.shape(&text, &under, text_color);
                    self.cursor.lines.push((origin, line));
                }
            }
            CursorShape::Vertical => {
                let width =
                    (self.cell.width * f32::from(mode.cell_percentage) / 100.).max(device_pixel);
                self.cursor.quads.push(fill(
                    Bounds::new(origin, size(width, self.cell.height)),
                    color,
                ));
            }
            CursorShape::Horizontal => {
                let height =
                    (self.cell.height * f32::from(mode.cell_percentage) / 100.).max(device_pixel);
                self.cursor.quads.push(fill(
                    Bounds::new(
                        point(origin.x, origin.y + self.cell.height - height),
                        size(width, height),
                    ),
                    color,
                ));
            }
        }
    }
}

impl Element for NvimElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let metrics = self.pane.update(cx, |pane, _| {
            let metrics = pane.metrics(window);
            let count = |extent: Pixels, cell: Pixels| {
                (f32::from(extent) / f32::from(cell))
                    .floor()
                    .clamp(1., f32::from(u16::MAX)) as u16
            };
            pane.set_grid_size((
                count(bounds.size.width, metrics.cell.width),
                count(bounds.size.height, metrics.cell.height),
            ));
            pane.layout = Some(GridLayout {
                origin: bounds.origin,
                cell: metrics.cell,
            });
            metrics
        });
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let pane = self.pane.read(cx);
        let focused = pane.focus_handle.is_focused(window);
        let background = pane.ui.highlight(0).bg;
        let mut painter = Painter {
            ui: &pane.ui,
            window,
            origin: bounds.origin,
            cell: metrics.cell,
            font: &metrics.font,
            scale: window.scale_factor(),
            grid: Layer::default(),
            cursor: Layer::default(),
        };
        painter.backgrounds(background);
        painter.text();
        painter.cursor(focused, pane.marked_text.as_deref());
        PrepaintState {
            hitbox,
            background,
            grid: painter.grid,
            cursor: painter.cursor,
            line_height: metrics.cell.height,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.pane.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            PaneInputHandler(ElementInputHandler::new(bounds, self.pane.clone())),
            cx,
        );
        window.set_cursor_style(gpui::CursorStyle::IBeam, &prepaint.hitbox);
        self.register_mouse_listeners(&prepaint.hitbox, window);

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, prepaint.background));
            prepaint.grid.paint(prepaint.line_height, window, cx);
            prepaint.cursor.paint(prepaint.line_height, window, cx);
        });
    }
}

impl NvimElement {
    fn register_mouse_listeners(&self, hitbox: &Hitbox, window: &mut Window) {
        let pane = self.pane.clone();
        let target = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && target.is_hovered(window) {
                pane.update(cx, |pane, cx| pane.mouse_down(event, window, cx));
            }
        });
        let pane = self.pane.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                pane.update(cx, |pane, _| pane.mouse_move(event));
            }
        });
        let pane = self.pane.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                pane.update(cx, |pane, _| pane.mouse_up(event));
            }
        });
        let pane = self.pane.clone();
        let target = hitbox.clone();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && target.should_handle_scroll(window) {
                pane.update(cx, |pane, _| pane.scroll_wheel(event));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guifont_lists_fonts_with_their_heights() {
        assert_eq!(
            parse_guifont("Iosevka:h14"),
            [("Iosevka".to_owned(), Some(14.))]
        );
        assert_eq!(
            parse_guifont(r"JetBrains\ Mono:b:h12.5,Menlo:h11,Fira\,Code"),
            [
                ("JetBrains Mono".to_owned(), Some(12.5)),
                ("Menlo".to_owned(), Some(11.)),
                ("Fira,Code".to_owned(), None),
            ]
        );
        assert_eq!(
            parse_guifont("SF Mono, Menlo"),
            [("SF Mono".to_owned(), None), ("Menlo".to_owned(), None)]
        );
        assert!(parse_guifont("").is_empty());
        assert!(parse_guifont("*").is_empty());
    }

    #[test]
    fn the_first_installed_font_wins() {
        let installed = |family: &str| ["Menlo", "Iosevka"].contains(&family);
        assert_eq!(
            choose_font(Some("SF Mono:h20,Iosevka:h15,Menlo"), installed),
            FontChoice {
                family: "Iosevka".into(),
                size: px(15.),
            }
        );
        let fallback = FontChoice {
            family: "Menlo".into(),
            size: px(DEFAULT_FONT_SIZE),
        };
        assert_eq!(choose_font(Some("Missing:h20"), installed), fallback);
        assert_eq!(choose_font(None, installed), fallback);
        assert_eq!(
            choose_font(None, |family| family == MONO_FAMILY),
            FontChoice {
                family: MONO_FAMILY.into(),
                size: px(DEFAULT_FONT_SIZE),
            },
            "the bundled font comes first when it resolves"
        );
    }

    #[test]
    fn rows_split_into_ascii_runs_and_single_wide_cells() {
        let cell = |text, hl| Cell { text, hl };
        let row = [
            cell(CellText::Char('a'), 1),
            cell(CellText::Char('b'), 1),
            cell(CellText::Char('c'), 2),
            cell(CellText::Char('界'), 2),
            cell(CellText::Continuation, 2),
            cell(CellText::Char('d'), 2),
            cell(CellText::Cluster("e\u{301}".into()), 2),
            cell(CellText::Char('é'), 2),
        ];
        let runs = row_runs(&row)
            .into_iter()
            .map(|run| (run.col, run.text, run.hl))
            .collect::<Vec<_>>();
        assert_eq!(
            runs,
            [
                (0, "ab".to_owned(), 1),
                (2, "c".to_owned(), 2),
                (3, "界".to_owned(), 2),
                (5, "d".to_owned(), 2),
                (6, "e\u{301}".to_owned(), 2),
                (7, "é".to_owned(), 2),
            ]
        );
    }
}
