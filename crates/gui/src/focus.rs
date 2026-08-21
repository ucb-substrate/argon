use gpui::App;

/// Remember the application that owns Nvim, then bring every GUI window to
/// the foreground. Remembering immediately before activation keeps this
/// working for both local Nvim and an SSH session running in a local terminal.
pub(crate) fn activate_gui(cx: &mut App) {
    platform::remember_editor();
    cx.activate(true);
    for window in cx.windows() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
}

/// Transfer focus back to the application that was frontmost before the GUI.
/// Returns `false` when the platform cannot identify or activate that app.
pub(crate) fn activate_editor() -> bool {
    platform::activate_editor()
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::atomic::{AtomicI32, Ordering};

    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

    static EDITOR_PID: AtomicI32 = AtomicI32::new(0);

    pub(super) fn remember_editor() {
        let Some(application) = NSWorkspace::sharedWorkspace().frontmostApplication() else {
            return;
        };
        let pid = application.processIdentifier();
        if pid > 0 && pid != std::process::id() as i32 {
            EDITOR_PID.store(pid, Ordering::Relaxed);
        }
    }

    pub(super) fn activate_editor() -> bool {
        let pid = EDITOR_PID.load(Ordering::Relaxed);
        if pid <= 0 {
            return false;
        }

        let Some(application) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        else {
            return false;
        };
        #[allow(deprecated)]
        application.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) fn remember_editor() {}

    pub(super) fn activate_editor() -> bool {
        false
    }
}
