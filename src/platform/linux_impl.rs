// The Linux backend.
//
// Audio is cpal, which speaks ALSA; PipeWire exposes an ALSA compatibility
// layer, so both the raw-ALSA and PipeWire worlds arrive through the same
// Audio impl.
//
// Typing is X11 XTest via enigo. On Wayland this reaches only XWayland
// clients; native Wayland windows need the input portal, which is not built
// yet. That limitation is stated here rather than papered over: type_text
// surfaces the failure instead of dropping words, and the release notes say
// the same thing.

use crate::platform::input::Injector;
use enigo::{Enigo, Keyboard, Settings};
use std::sync::Mutex;

/// Text injection through XTest. XWayland sessions work; native Wayland
/// windows will refuse injection until the portal path exists.
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
        let outcome = slot.as_mut().expect("populated above").text(text);
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                // The connection is no longer usable, so it is dropped and the
                // next commit opens a fresh one instead of failing for the rest
                // of the session.
                *slot = None;
                Err(format!("text injection failed ({e})"))
            }
        }
    }
}