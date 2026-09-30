// The macOS backend.
//
// Audio is cpal over CoreAudio. Typing is CoreGraphics event synthesis through
// enigo, which macOS permits only after the user grants the binary the
// Accessibility permission (System Settings > Privacy & Security >
// Accessibility); Input Monitoring governs event taps, which is a different
// API from the synthetic events this path posts. CGEvent::post returns nothing
// and enigo discards the result, so without the grant every word would be
// posted and dropped while type_text still reported success. The grant is
// therefore probed directly and a commit without it fails with an error naming
// the setting, which is the best a CLI binary can do - an app-bundle build
// could prompt, and that is the follow-up.

use crate::platform::input::Injector;
use enigo::{Enigo, Keyboard, Settings};
use std::cell::RefCell;
use std::thread;
use std::time::Duration;

// Whether this process may post synthetic keyboard events at all.
//
// macOS answers from the Accessibility permission database. It is read before
// every commit rather than cached, so granting the permission takes effect
// without a restart.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

thread_local! {
    /// One event source per typing thread rather than one per commit.
    ///
    /// Constructing an `Enigo` creates a `CGEventSource`, reads the main
    /// display and reads the system's double-click interval, and the streaming
    /// loop's reader thread calls `type_text` once per batch of words it
    /// commits, so a per-call rebuild pays that hundreds of times over one
    /// dictation. The cache is per thread because a `CGEventSource` is a
    /// CoreFoundation handle that cannot be shared across threads the way the
    /// Linux connection can.
    static ENIGO: RefCell<Option<Enigo>> = RefCell::new(None);
}

/// Pause between two synthesized characters.
///
/// The injector contract in `crate::platform::input` is one event per character
/// with a small gap; the reference is the Windows engine's 2 ms inter-keystroke
/// sleep in `crate::paste`. A burst delivered back to back can outpace the
/// target window's message queue and drop characters.
const KEY_GAP: Duration = Duration::from_millis(2);

/// Text injection through CoreGraphics events, gated by the Accessibility
/// permission.
pub fn default_injector() -> &'static dyn Injector {
    use std::sync::OnceLock;
    static INJECTOR: OnceLock<CgInjector> = OnceLock::new();
    INJECTOR.get_or_init(|| CgInjector)
}

struct CgInjector;

impl Injector for CgInjector {
    fn type_text(&self, text: &str) -> Result<(), String> {
        if !unsafe { AXIsProcessTrusted() } {
            return Err(
                "macOS requires the Accessibility permission for this binary \
                 (System Settings > Privacy & Security > Accessibility)"
                    .to_string(),
            );
        }
        // One event per character, the contract every backend keeps: the
        // Windows engine sends a KEYEVENTF_UNICODE pair per character with a
        // 2 ms gap, while enigo's `text` batches up to twenty characters into
        // a single CGEvent, so each character is handed to it on its own.
        for c in text.chars() {
            let outcome = ENIGO.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(
                        Enigo::new(&Settings::default())
                            .map_err(|e| format!("cannot initialise input injection ({e})"))?,
                    );
                }
                slot.as_mut()
                    .expect("populated above")
                    .text(&c.to_string())
                    .map_err(|e| format!("text injection failed ({e})"))
            });
            if let Err(e) = outcome {
                // The event source is no longer usable, so it is dropped and
                // the next commit opens a fresh one instead of failing for the
                // rest of the session.
                ENIGO.with(|slot| *slot.borrow_mut() = None);
                return Err(e);
            }
            thread::sleep(KEY_GAP);
        }
        Ok(())
    }
}
