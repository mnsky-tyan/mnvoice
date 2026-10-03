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
use std::thread;

use crate::platform::input::KEY_GAP;

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
        let mut slot = self.enigo.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Enigo::new(&Settings::default()).map_err(|e| {
                format!("cannot initialise input injection ({e}); is an X display available?")
            })?);
        }
        // enigo already posts one event per character here: its x11rb backend
        // has no fast text entry, so `text` falls back to one key event per
        // character. What it cannot be asked for is the gap, so the characters
        // are handed to it one at a time to add the pacing the injector
        // contract in `crate::platform::input` requires - the 2 ms the Windows
        // engine sleeps in `crate::paste` is the reference value.
        for c in text.chars() {
            let outcome = slot.as_mut().expect("populated above").text(&c.to_string());
            if let Err(e) = outcome {
                // The connection is no longer usable, so it is dropped and the
                // next commit opens a fresh one instead of failing for the rest
                // of the session.
                *slot = None;
                return Err(format!("text injection failed ({e})"));
            }
            thread::sleep(KEY_GAP);
        }
        Ok(())
    }
}
