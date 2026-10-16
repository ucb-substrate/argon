pub const ZED_PLEX_MONO: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/plex-mono/ZedPlexMono-Regular.ttf"
));

pub const ZED_PLEX_MONO_BOLD: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/plex-mono/ZedPlexMono-Bold.ttf"
));

pub const ZED_PLEX_MONO_ITALIC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/plex-mono/ZedPlexMono-Italic.ttf"
));

pub const ZED_PLEX_MONO_BOLD_ITALIC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/plex-mono/ZedPlexMono-BoldItalic.ttf"
));

/// The family name of the bundled monospace font.
pub const ZED_PLEX_MONO_FAMILY: &str = "Zed Plex Mono";

pub const ZED_PLEX_SANS: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/plex-sans/ZedPlexSans-Regular.ttf"
));
