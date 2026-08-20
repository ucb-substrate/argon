use gpui::actions;

// Associate actions using the `actions!` macro (or `impl_actions!` macro)
actions!(
    Argon,
    [
        Quit,
        DrawRect,
        SelectMode,
        DrawDim,
        Edit,
        Fit,
        Zero,
        One,
        All,
        EditDim,
        Undo,
        Redo,
        Command,
        Cancel,
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Enter,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        DarkMode,
        LightMode,
    ]
);
