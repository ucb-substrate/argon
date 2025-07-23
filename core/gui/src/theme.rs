use gpui::{Rgba, rgb};
use lazy_static::lazy_static;

pub struct Theme {
    pub titlebar: Rgba,
    pub sidebar: Rgba,
    pub bg: Rgba,
    pub divider: Rgba,
}

lazy_static! {
    pub static ref THEME: Theme = Theme {
        titlebar: rgb(0x1f2430),
        sidebar: rgb(0x0f2430),
        bg: rgb(0x002430),
        divider: rgb(0x91969E),
    };
}
