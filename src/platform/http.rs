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

impl Response {
    /// The error for a status the caller does not accept: the code plus a
    /// bounded preview of the body, so a provider's explanation survives
    /// without a megabyte of HTML in the log.
    pub fn error_for_status(&self, what: &str) -> String {
        let preview: String = String::from_utf8_lossy(&self.body).chars().take(200).collect();
        format!("{what} returned HTTP {}: {preview}", self.status)
    }
}

/// A blocking WebSocket client for the streaming transcription path.
///
/// Implementations MUST allow `read` on one thread to proceed while `send`
/// is in flight on another: the streaming loop types words from a reader
/// thread while the main thread keeps pushing 40 ms audio packets, and a
/// socket that serializes those would stop sending audio whenever the
/// provider goes quiet. WinHTTP's socket handle is naturally full duplex;
/// backends built on a single TCP stream get there with a short read timeout
/// so the lock is never held long.
///
/// The concrete type differs per platform - WinHTTP's WebSocket on Windows, a
/// pure-Rust client elsewhere - so this is a trait rather than a struct. The
/// methods are the minimum the streaming loop actually uses: send a binary
/// frame, send a text frame, and read one frame with a timeout.
pub trait WebSocket: Send + Sync {
    /// Send a binary frame (a slice of PCM audio).
    fn send_binary(&self, data: &[u8]) -> Result<(), String>;

    /// Send a text frame (the provider's config message).
    fn send_text(&self, text: &str) -> Result<(), String>;

    /// Send a close frame and shut down without waiting for the peer.
    fn close(&self);

    /// Read the next frame, returning `None` on a clean close.
    ///
    /// Reads never block indefinitely - that is what keeps the streaming loop
    /// responsive to the user releasing the hotkey: a blocking read with no
    /// timeout would hold the worker until the provider decided to speak, so
    /// the transcript would arrive long after the user stopped talking. The
    /// Unix backend polls on a socket read timeout, the Windows backend times
    /// out on the whole WinHTTP request; the loop re-checks its flags between
    /// calls. A backend that cannot vary this per read should say so in its
    /// implementation.
    fn read(&self) -> Result<Option<Vec<u8>>, String>;
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

    /// A POST with a raw body. Used by the REST transcription fallback, which
    /// uploads a WAV as multipart/form-data.
    ///
    /// `auth` is the full `Authorization` header value when the endpoint needs
    /// one - providers disagree on the scheme (Deepgram wants `Token`, OpenAI
    /// compatible endpoints want `Bearer`), so the caller owns the prefix and
    /// the transport only puts it on the wire.
    fn post(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> Result<Response, String>;

    /// Open a WebSocket to `url`, sending the given headers on the handshake.
    ///
    /// `url` is a `ws://` or `wss://` endpoint. TLS is handled inside each
    /// backend so the caller never has to think about certificate stores.
    fn websocket(&self, url: &str, headers: &[(&str, &str)]) -> Result<Box<dyn WebSocket>, String>;
}

/// The transport for this build.
///
/// Windows has its own WinHTTP client; Linux and macOS share one pure-Rust
/// client whose TLS stack is chosen per target by Cargo.toml's features. Linux
/// installs rustls' crypto provider through `unix_http::ensure_tls_ready`;
/// macOS gets Security.framework through native-tls, whose connector
/// `unix_http::agent_builder` installs on each agent.
///
/// Each backend pins the redirect contract through this name in its own tests:
/// Windows in update.rs, where the updater is the caller, and Unix in
/// unix_http.rs, where the backend lives.
#[cfg(windows)]
pub use crate::platform::windows_impl::WinHttpTransport as NativeTransport;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use crate::platform::unix_http::UnixTransport as NativeTransport;
