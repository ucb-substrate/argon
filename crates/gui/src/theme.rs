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
    /// Schematic symbols and their text.
    pub symbol: Rgba,
    pub wire: Rgba,
    pub pin: Rgba,
    pub flyline: Rgba,
    pub conflict: Rgba,
    /// Elements and wires the GUI cannot edit.
    pub locked: Rgba,
    pub hover: Rgba,
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
        symbol: rgb(0x222222),
        wire: rgb(0x1f5fbf),
        pin: rgb(0xd0471d),
        flyline: rgb(0x8a8a8a),
        conflict: rgb(0xe00000),
        locked: rgb(0x9a9a9a),
        hover: rgba(0x7236ff33),
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
        symbol: rgb(0xdddddd),
        wire: rgb(0x6fb1ff),
        pin: rgb(0xff9f5a),
        flyline: rgb(0x9a9a9a),
        conflict: rgb(0xff5555),
        locked: rgb(0x777777),
        hover: rgba(0x7236ff44),
    };
}
