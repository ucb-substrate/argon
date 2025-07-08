use std::ops::Range;

use gpui::{
    div, yellow, Context, Entity, HighlightStyle, IntoElement, ParentElement, Render, Styled,
    StyledText, Subscription, Window,
};

use crate::project::ProjectState;

pub struct TextDisplay {
    pub text: String,
    pub highlight_span: Option<Range<usize>>,
    pub subscriptions: Vec<Subscription>,
}

impl TextDisplay {
    pub fn new(cx: &mut Context<Self>, state: &Entity<ProjectState>) -> Self {
        let subscriptions = vec![cx.observe(state, |this, state, cx| {
            let proj_state = state.read(cx);
            this.text = proj_state.code.clone();
            this.highlight_span = proj_state
                .selected_rect
                .and_then(|i| Some(proj_state.solved_cell.rects[i].attrs.source.as_ref()?.span))
                .map(|span| span.start()..span.end());
            cx.notify();
        })];
        let proj_state = state.read(cx);
        Self {
            text: proj_state.code.clone(),
            highlight_span: None,
            subscriptions,
        }
    }
}

impl Render for TextDisplay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(span) = self.highlight_span.clone() {
            let highlight = HighlightStyle::color(yellow());
            let text = StyledText::new(&self.text).with_highlights([(span, highlight)]);
            div().w_1_3().child(text)
        } else {
            div().w_1_3().child(self.text.clone())
        }
    }
}
