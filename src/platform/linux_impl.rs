// The Linux backend.
//
// Audio is cpal, which speaks ALSA; PipeWire exposes an ALSA compatibility
// layer, so both the raw-ALSA and PipeWire worlds arrive through the same
// Audio impl.
//
// Typing is X11 XTest via enigo, so injected keys reach X11 and XWayland
// windows only. A native Wayland window receives nothing and nothing on this
// path can detect that, so the limitation is stated here rather than papered
// over: a missing X display is the only failure the binary reports, and the
// guard is to use an X session or wait for the input portal.

use crate::platform::input::Injector;
use enigo::{Enigo, Keyboard, Settings};
use std::sync::Mutex;

/// Text injection through XTest. X11 and XWayland windows receive the keys; a
/// native Wayland window receives nothing, and nothing here can detect that.
pub fn default_injector() -> &'static dyn Injector {
    use std::sync::OnceLock;
    static INJECTOR: OnceLock<X11Injector> = OnceLock::new();
    INJECTOR.get_or_init(|| X11Injector {
        enigo: Mutex::new(None),
    })
}

/// Holds the X connection rather than rebuilding it per call.
///
/// Constructing an `Enigo` connects to the X server and reads the keyboard and
/// modifier maps, and the streaming loop's reader thread calls `type_text` once
/// per batch of words it commits, so a per-call rebuild pays that round trip
/// hundreds of times over one dictation. The connection is built on first use
/// instead, which is the once-per-process cost the Windows injector pays.
struct X11Injector {
    enigo: Mutex<Option<Enigo>>,
}

impl Injector for X11Injector {
    fn type_text(&self, text: &str) -> Result<(), String> {
        let mut slot = self
            .enigo
            .lock()
            .map_err(|_| "text injection lock poisoned".to_string())?;
        if slot.is_none() {
            *slot = Some(Enigo::new(&Settings::default()).map_err(|e| {
                format!("cannot initialise input injection ({e}); is an X display available?")
            })?);
        }
        // enigo already posts one event per character here: its x11rb backend
        // has no fast text entry, so `text` falls back to one key event per
        // character. What it cannot be asked for is the gap, so the pacing
        // (one character per event, `KEY_GAP` after each) comes from
        // `input::type_paced`, the contract's single owner - the 2 ms the
        // Windows engine sleeps in `crate::paste` is the reference value.
        // On failure the connection is no longer usable, so it is dropped and
        // the next commit opens a fresh one instead of failing for the rest of
        // the session; the pacer stops at the first error either way.
        crate::platform::input::type_paced(text, |c| {
            let outcome = slot.as_mut().expect("populated above").text(&c.to_string());
            if let Err(e) = outcome {
                // The borrow above has ended by here, so the slot can be cleared.
                *slot = None;
                return Err(format!("text injection failed ({e})"));
            }
            Ok(())
        })
    }
}
