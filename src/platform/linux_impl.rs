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

use crate::platform::audio::Audio;
use crate::platform::http::Transport;
use crate::platform::input::Injector;

/// Audio capture through ALSA / PipeWire (via its ALSA layer).
pub fn audio() -> Result<impl Audio, String> {
    crate::platform::unix_audio::CpalAudio::new()
}

/// Network transport. See unix_http for why TLS stacks differ per platform.
pub struct LinuxTransport;

impl Transport for LinuxTransport {
    fn get(&self, url: &str, accept: &str) -> Result<crate::platform::http::Response, String> {
        crate::platform::unix_http::get(url, accept)
    }

    fn post(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> Result<crate::platform::http::Response, String> {
        crate::platform::unix_http::post(url, auth, content_type, body)
    }

    fn websocket(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Box<dyn crate::platform::http::WebSocket>, String> {
        Ok(Box::new(crate::platform::unix_http::websocket(url, headers)?))
    }
}

/// Text injection through XTest. XWayland sessions work; native Wayland
/// windows will refuse injection until the portal path exists.
pub fn default_injector() -> &'static dyn Injector {
    use std::sync::OnceLock;
    static INJECTOR: OnceLock<X11Injector> = OnceLock::new();
    INJECTOR.get_or_init(X11Injector)
}

struct X11Injector;

impl Injector for X11Injector {
    fn type_text(&self, text: &str) -> Result<(), String> {
        use enigo::{Enigo, Keyboard, Settings};
        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("cannot initialise input injection ({e}); is an X display available?"))?;
        enigo
            .text(text)
            .map_err(|e| format!("text injection failed ({e})"))
    }
}