/// The family name of the bundled interface font.
pub const SANS_FAMILY: &str = "IBM Plex Sans";

/// The family name of the bundled monospace font.
pub const MONO_FAMILY: &str = "Lilex";

macro_rules! font {
    ($path:literal) => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/", $path))
    };
}

/// Every bundled font file.
pub const FONTS: [&[u8]; 8] = [
    font!("ibm-plex-sans/IBMPlexSans-Regular.ttf"),
    font!("ibm-plex-sans/IBMPlexSans-Italic.ttf"),
    font!("ibm-plex-sans/IBMPlexSans-SemiBold.ttf"),
    font!("ibm-plex-sans/IBMPlexSans-SemiBoldItalic.ttf"),
    font!("lilex/Lilex-Regular.ttf"),
    font!("lilex/Lilex-Bold.ttf"),
    font!("lilex/Lilex-Italic.ttf"),
    font!("lilex/Lilex-BoldItalic.ttf"),
];
