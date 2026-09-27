// One GET, one POST, one WebSocket handshake - the whole transport surface.
//
// This exists as a seam rather than being called directly because the Windows
// build talks to the network through WinHTTP, and WinHTTP happens to follow
// GitHub's 302 redirect by default. The updater depends on that: release assets
// are served from a CDN, and a client that stops at the redirect downloads a
// short HTML page instead of an executable. Since "the library follows it" is
// not a property the code stated, it is stated here and asserted by a test that
// would fail if either backend stopped following it.
//
// The API is deliberately blocking and tiny. mnvoice has no async runtime and
// does not want one - the capture path runs on dedicated OS threads with
// explicit channels, and adding a reactor would put a scheduler between the
// microphone and the socket for no benefit.

/// Response to a completed request. The status is carried rather than turned
/// into an error here, because the callers disagree about which codes are
/// interesting: the update path wants to say "HTTP 404" in its message, while
/// the REST path treats any non-200 as a transcription failure.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A blocking WebSocket client for the streaming transcription path.
///
/// The concrete type differs per platform - WinHTTP's WebSocket on Windows, a
/// pure-Rust client elsewhere - so this is a trait rather than a struct. The
/// methods are the minimum the streaming loop actually uses: send a binary
/// frame, send a text frame, and read one frame with a timeout.
pub trait WebSocket {
    /// Send a binary frame (a slice of PCM audio).
    fn send_binary(&mut self, data: &[u8]) -> Result<(), String>;

    /// Send a text frame (the provider's config message).
    fn send_text(&mut self, text: &str) -> Result<(), String>;

    /// Send a close frame and shut down without waiting for the peer.
    fn close(&mut self);

    /// Read the next frame, returning `None` on a clean close.
    ///
    /// `timeout_ms` is what keeps the streaming loop responsive to the user
    /// releasing the hotkey: a blocking read with no timeout would hold the
    /// worker until the provider decided to speak, so the transcript would
    /// arrive long after the user stopped talking.
    fn read(&mut self, timeout_ms: u32) -> Result<Option<Vec<u8>>, String>;
}

/// The whole network surface, one implementation per platform.
///
/// Taking this as a trait rather than a set of free functions keeps every
/// backend behind one object, so a caller cannot accidentally use the Windows
/// client on Linux by importing the wrong name, and so tests can substitute a
/// transport without opening a socket.
pub trait Transport {
    /// A GET with an `Accept` header, following redirects.
    ///
    /// Following the redirect is the contract, not an implementation detail. It
    /// is documented here so a future backend swap has to consciously preserve
    /// it, and asserted by a test against a real redirecting loopback endpoint.
    fn get(&self, url: &str, accept: &str) -> Result<Response, String>;

    /// A POST with a raw body and a Content-Type. Used by the REST transcription
    /// fallback, which uploads a WAV as multipart/form-data.
    fn post(&self, url: &str, content_type: &str, body: &[u8]) -> Result<Response, String>;

    /// Open a WebSocket to `url`, sending the given headers on the handshake.
    ///
    /// `url` is a `ws://` or `wss://` endpoint. TLS is handled inside each
    /// backend so the caller never has to think about certificate stores.
    fn websocket(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Box<dyn WebSocket>, String>;
}

/// The transport for this build.
///
/// Linux and macOS each name their own type rather than sharing a `unix_impl`
/// module, because the two differ in something that matters: on Linux rustls
/// has no default crypto provider, so the backend has to install one before the
/// first handshake, while macOS can lean on the platform's own verifier. A
/// shared module would have to `#[cfg]` that difference anyway, so the split is
/// honest about where the real difference is.
#[cfg(windows)]
pub use crate::platform::windows_impl::WinHttpTransport as NativeTransport;

#[cfg(target_os = "linux")]
pub use crate::platform::linux_impl::LinuxTransport as NativeTransport;

#[cfg(target_os = "macos")]
pub use crate::platform::macos_impl::MacTransport as NativeTransport;