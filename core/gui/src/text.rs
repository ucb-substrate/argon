use std::ops::Range;

use gpui::{div, green, Context, IntoElement, ParentElement, Render, Styled, Window};

pub struct TextDisplay {
    pub text: String,
    pub highlight_span: Option<Range<usize>>,
}

impl Render for TextDisplay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(span) = self.highlight_span.clone() {
            let pre = self.text[0..span.start].to_string();
            let highlight = self.text[span.clone()].to_string();
            let post = self.text[span.end..].to_string();
            let inner = div().text_color(green()).child(highlight);
            div().w_full().child(div().flex().w_full().child(pre).child(inner).child(post))
        } else {
            div().w_full().child(self.text.clone())
        }
    }
}
