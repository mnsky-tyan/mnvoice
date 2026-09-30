// The keyboard injection contract.
//
// mnvoice's defining behaviour is typing into whatever window has focus, as
// words arrive. The three platforms disagree sharply about what that is allowed
// to mean: Windows `SendInput` synthesizes keystrokes globally; X11 allows it
// through XTest; macOS requires the user to grant Accessibility; Wayland
// forbids it outright unless the compositor's input portal consents. Those are
// capability differences, not spelling differences, which is why this is a
// trait rather than a cfg'd function.
//
// One character per synthesized event with a small gap, rather than a single
// giant paste, is deliberate and must be preserved by every backend: pasting
// replaces the user's clipboard, and a burst of keystrokes delivered in one
// SendInput call can outpace the target window's message queue and drop
// characters.

/// Types `text` into the currently focused window, as if the user typed it.
pub trait Injector: Send + Sync {
    /// Type the whole string. Returns an error only when injection is
    /// unavailable - a refused permission, no display, a compositor that said
    /// no - so the caller can surface that instead of failing silently.
    fn type_text(&self, text: &str) -> Result<(), String>;
}

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static INJECTOR: OnceLock<&'static dyn Injector> = OnceLock::new();
static FAILING: AtomicBool = AtomicBool::new(false);

/// The platform's injector, installing the default on first use.
///
/// Shared code (the streaming loop) types through this rather than naming a
/// backend, which is what lets the same streaming loop type on Windows, X11
/// and macOS without a single cfg in its body.
pub fn global() -> &'static dyn Injector {
    *INJECTOR.get_or_init(|| {
        #[cfg(windows)]
        {
            &crate::platform::windows_impl::SEND_INPUT_INJECTOR
        }
        #[cfg(target_os = "linux")]
        {
            crate::platform::linux_impl::default_injector()
        }
        #[cfg(target_os = "macos")]
        {
            crate::platform::macos_impl::default_injector()
        }
    })
}

/// Convenience matching the pre-seam call sites: type, and report a failure
/// instead of dropping words silently. The streaming loop calls this once per
/// commit, and every one of those call sites discards the result, so this is
/// the only signal a typed-away transcript ever produces.
///
/// The rule is a rising edge, not one report per process: a failure is printed
/// when it starts and again after any recovery, so a streak of failing commits
/// is announced once while a backend that recovers between commits is never
/// silently mute for the rest of the process.
pub fn type_text(text: &str) {
    match global().type_text(text) {
        Ok(()) => {
            FAILING.store(false, Ordering::SeqCst);
        }
        Err(e) => {
            if !FAILING.swap(true, Ordering::SeqCst) {
                // Written through `writeln!` with the result dropped, not
                // `eprintln!`: the Windows tray build has no stderr to write to,
                // and a failed print there panics instead of staying silent.
                let _ = writeln!(std::io::stderr(), "mnvoice: {e}");
            }
        }
    }
}
