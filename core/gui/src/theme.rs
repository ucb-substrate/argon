use gpui::{Rgba, rgb, rgba};
use lazy_static::lazy_static;

pub struct Theme {
    pub titlebar: Rgba,
    pub sidebar: Rgba,
    pub bg: Rgba,
    pub divider: Rgba,
    pub text: Rgba,
    pub selection: Rgba,
    pub input_bg: Rgba,
    pub axes: Rgba,
    pub error: Rgba,
    pub subtext: Rgba,
}

lazy_static! {
    pub static ref LIGHT_THEME: Theme = Theme {
        titlebar: rgb(0xEEEEEE),
        sidebar: rgb(0xFFFFFF),
        bg: rgb(0xFFFFFF),
        divider: rgb(0xDDDDDD),
        text: rgb(0x0),
        selection: rgba(0x7236ff22),
        input_bg: rgb(0xEEEEEE),
        axes: rgb(0xcdb8ff),
        error: rgb(0xff00000),
        subtext: rgb(0x555555),
    };
    pub static ref DARK_THEME: Theme = Theme {
        titlebar: rgb(0x1a1a1a),
        sidebar: rgb(0x202020),
        bg: rgb(0x202020),
        divider: rgb(0x505050),
        text: rgb(0xCCCCCC),
        selection: rgba(0x7236ff66),
        input_bg: rgb(0x1a1a1a),
        axes: rgb(0x7236ff),
        error: rgb(0xff00000),
        subtext: rgb(0x999999),
    };
}
