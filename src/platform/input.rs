// The keyboard injection contract.
//
// mnvoice's defining behaviour is typing into whatever window has focus, as
// words arrive. The three platforms disagree sharply about what that is allowed
// to mean: Windows `SendInput` synthesizes keystrokes globally; X11 allows it
// through XTest; macOS requires the user to grant Input Monitoring; Wayland
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