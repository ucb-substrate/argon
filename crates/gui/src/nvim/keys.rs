//! Translation from GPUI keystrokes to Neovim key notation.

use gpui::Keystroke;

/// The Neovim name of a key that never types text by itself.
fn special_key_name(key: &str) -> Option<String> {
    let name = match key {
        "escape" => "Esc",
        "enter" => "CR",
        "tab" => "Tab",
        "backspace" => "BS",
        "delete" => "Del",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        _ => {
            let number = key
                .strip_prefix('f')?
                .parse::<u8>()
                .ok()
                .filter(|number| (1..=35).contains(number))?;
            return Some(format!("F{number}"));
        }
    };
    Some(name.to_owned())
}

/// The keys to send for `keystroke`, or `None` when it types text.
pub(crate) fn to_nvim_keys(keystroke: &Keystroke, alt_is_meta: bool) -> Option<String> {
    let modifiers = &keystroke.modifiers;
    let mods = [
        (modifiers.control, "C-"),
        (modifiers.alt, "M-"),
        (modifiers.platform, "D-"),
        (modifiers.shift, "S-"),
    ]
    .into_iter()
    .filter_map(|(held, prefix)| held.then_some(prefix))
    .collect::<String>();
    let key = keystroke.key.as_str();
    if let Some(name) = special_key_name(key) {
        return Some(format!("<{mods}{name}>"));
    }
    if key == "space" && !mods.is_empty() {
        return Some(format!("<{mods}Space>"));
    }
    if modifiers.control || modifiers.platform || (modifiers.alt && alt_is_meta) {
        let key = match key {
            "<" => "lt",
            "\\" => "Bslash",
            "|" => "Bar",
            key => key,
        };
        return Some(format!("<{mods}{key}>"));
    }
    None
}

#[cfg(test)]
mod tests {
    use gpui::Keystroke;

    use super::to_nvim_keys;

    fn keys(keystroke: &str, alt_is_meta: bool) -> Option<String> {
        to_nvim_keys(&Keystroke::parse(keystroke).unwrap(), alt_is_meta)
    }

    #[test]
    fn special_keys_have_neovim_names() {
        for (key, name) in [
            ("escape", "<Esc>"),
            ("enter", "<CR>"),
            ("tab", "<Tab>"),
            ("backspace", "<BS>"),
            ("delete", "<Del>"),
            ("insert", "<Insert>"),
            ("home", "<Home>"),
            ("end", "<End>"),
            ("pageup", "<PageUp>"),
            ("pagedown", "<PageDown>"),
            ("up", "<Up>"),
            ("down", "<Down>"),
            ("left", "<Left>"),
            ("right", "<Right>"),
            ("f1", "<F1>"),
            ("f12", "<F12>"),
            ("shift-tab", "<S-Tab>"),
            ("ctrl-space", "<C-Space>"),
        ] {
            assert_eq!(keys(key, false).as_deref(), Some(name), "{key}");
        }
        assert_eq!(keys("space", false), None, "a plain space is text");
    }

    #[test]
    fn modifiers_are_written_in_neovim_order() {
        assert_eq!(keys("ctrl-w", false).as_deref(), Some("<C-w>"));
        assert_eq!(keys("cmd-s", false).as_deref(), Some("<D-s>"));
        assert_eq!(
            keys("ctrl-alt-cmd-shift-up", false).as_deref(),
            Some("<C-M-D-S-Up>")
        );
        assert_eq!(keys("ctrl-shift-a", false).as_deref(), Some("<C-S-a>"));
    }

    #[test]
    fn brackets_backslash_and_bar_are_escaped() {
        assert_eq!(keys("ctrl-<", false).as_deref(), Some("<C-lt>"));
        assert_eq!(keys("ctrl-\\", false).as_deref(), Some("<C-Bslash>"));
        assert_eq!(keys("cmd-|", false).as_deref(), Some("<D-Bar>"));
    }

    #[test]
    fn alt_is_meta_only_when_asked() {
        assert_eq!(keys("alt-x", true).as_deref(), Some("<M-x>"));
        assert_eq!(keys("alt-x", false), None);
        assert_eq!(keys("alt-left", false).as_deref(), Some("<M-Left>"));
    }

    #[test]
    fn plain_text_is_left_to_the_input_handler() {
        for key in ["a", "shift-a", "1", "<", ":", "\\"] {
            assert_eq!(keys(key, true), None, "{key}");
        }
    }
}
