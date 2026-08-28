use gpui::App;

/// Environment variable used to pass the application that invoked `argone`
/// through Nvim/analyzer subprocesses to the GUI.
pub const TARGET_ENV: &str = "ARGONE_FOCUS_TARGET";

/// Capture the application in which the top-level `argone` command is running.
/// The opaque value is only interpreted by the same platform's GUI process.
pub fn capture_target() -> Option<String> {
    platform::capture_target()
}

/// Initialize the GUI's return-focus target once. When the GUI was launched
/// without the top-level `argone` command, the currently frontmost application
/// is the best available fallback.
pub(crate) fn initialize_target() {
    platform::initialize_target(std::env::var(TARGET_ENV).ok().as_deref());
}

/// Bring every GUI window to the foreground.
pub(crate) fn activate_gui(cx: &mut App) {
    cx.activate(true);
    for window in cx.windows() {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
}

/// Transfer focus back to the application that invoked `argone`.
/// Returns `false` when the platform cannot identify or activate that app.
pub(crate) fn activate_invoker() -> bool {
    platform::activate_invoker()
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::atomic::{AtomicI32, Ordering};

    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication, NSWorkspace};

    static INVOKER_PID: AtomicI32 = AtomicI32::new(0);

    pub(super) fn capture_target() -> Option<String> {
        let application = NSWorkspace::sharedWorkspace().frontmostApplication()?;
        let pid = application.processIdentifier();
        (pid > 0 && pid != std::process::id() as i32).then(|| pid.to_string())
    }

    pub(super) fn initialize_target(configured: Option<&str>) {
        let pid = configured
            .and_then(|target| target.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
            .or_else(|| capture_target().and_then(|target| target.parse().ok()));
        if let Some(pid) = pid {
            let _ = INVOKER_PID.compare_exchange(0, pid, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    pub(super) fn activate_invoker() -> bool {
        let pid = INVOKER_PID.load(Ordering::Relaxed);
        if pid <= 0 {
            return false;
        }

        let Some(application) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        else {
            return false;
        };
        #[expect(
            deprecated,
            reason = "ignoring other apps still guarantees activation on macOS versions before 14"
        )]
        application.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) fn capture_target() -> Option<String> {
        None
    }

    pub(super) fn initialize_target(_configured: Option<&str>) {}

    pub(super) fn activate_invoker() -> bool {
        false
    }
}
