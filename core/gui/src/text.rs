use std::ops::Range;

use gpui::{
    div, green, Context, HighlightStyle, IntoElement, ParentElement, Render, Styled, StyledText,
    Window,
};

pub struct TextDisplay {
    pub text: String,
    pub highlight_span: Option<Range<usize>>,
}

impl Render for TextDisplay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(span) = self.highlight_span.clone() {
            let highlight = HighlightStyle::color(green());
            let text = StyledText::new(&self.text).with_highlights([(span, highlight)]);
            div().w_full().child(text)
        } else {
            div().w_full().child(self.text.clone())
        }
    }
}
