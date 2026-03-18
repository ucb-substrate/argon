use eframe::egui::Color32;

#[derive(Clone, Copy)]
pub struct Theme {
    pub titlebar: Color32,
    pub bg: Color32,
    pub text: Color32,
    pub selection: Color32,
    pub input_bg: Color32,
    pub axes: Color32,
    pub error: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

pub fn light_theme() -> Theme {
    Theme {
        titlebar: rgb(0xeeeeee),
        bg: rgb(0xffffff),
        text: rgb(0x000000),
        selection: Color32::from_rgba_unmultiplied(0x72, 0x36, 0xff, 0x22),
        input_bg: rgb(0xeeeeee),
        axes: rgb(0xcdb8ff),
        error: rgb(0xff0000),
    }
}

pub fn dark_theme() -> Theme {
    Theme {
        titlebar: rgb(0x1a1a1a),
        bg: rgb(0x202020),
        text: rgb(0xcccccc),
        selection: Color32::from_rgba_unmultiplied(0x72, 0x36, 0xff, 0x66),
        input_bg: rgb(0x1a1a1a),
        axes: rgb(0x7236ff),
        error: rgb(0xff0000),
    }
}
