// The macOS backend.
//
// Audio is cpal over CoreAudio. Typing is CoreGraphics event synthesis through
// enigo, which macOS permits only after the user grants Input Monitoring to
// the binary (System Settings > Privacy & Security). The first injection
// attempt without the grant fails; the error names the permission rather than
// dropping words silently, which is the best a CLI binary can do - an
// app-bundle build could prompt, and that is the follow-up.

use crate::platform::audio::Audio;
use crate::platform::input::Injector;

/// Audio capture through CoreAudio.
pub fn audio() -> Result<impl Audio, String> {
    crate::platform::unix_audio::CpalAudio::new()
}

/// Text injection through CoreGraphics events, gated by the Input Monitoring
/// permission.
pub fn default_injector() -> &'static dyn Injector {
    use std::sync::OnceLock;
    static INJECTOR: OnceLock<CgInjector> = OnceLock::new();
    INJECTOR.get_or_init(CgInjector)
}

struct CgInjector;

impl Injector for CgInjector {
    fn type_text(&self, text: &str) -> Result<(), String> {
        let mut enigo = enigo::Enigo::new(&enigo::Settings::default())
            .map_err(|e| format!("cannot initialise input injection ({e})"))?;
        enigo
            .text(text)
            .map_err(|_| {
                "text injection failed; macOS requires Input Monitoring permission for \
                 this binary (System Settings > Privacy & Security > Input Monitoring)"
                    .to_string()
            })
    }
}