// Blocking HTTP and WebSocket over TLS, shared by the Linux and macOS
// backends.
//
// The shape is dictated by the seam's contract, which is dictated by the
// updater: GET follows redirects (release assets are served from a CDN after
// a 302), status codes ride in `Response` instead of raising, and the socket
// is genuinely full duplex - the streaming loop types words on a reader
// thread while the main thread pushes audio.
//
// The two backends differ only in TLS stack, which is decided by their
// Cargo.toml features: Linux uses rustls with the ring provider and Mozilla
// root certificates bundled at build time (Linux has no single trusted root
// store to query), macOS uses native-tls, which is Security.framework and
// therefore the platform's own verifier and keychain trust. The one line that
// differs - installing rustls' crypto provider, which rustls refuses to pick
// a default for - is `ensure_tls_ready`, empty on macOS.
//
// Like the Windows transport, every call is stateless: open, complete, close.
// A failed request cannot poison the next one.

use crate::platform::http::{Response, WebSocket};
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

// tungstenite re-exports the http types its handshake needs.
use tungstenite::http as http;
use tungstenite::WebSocket as WsRaw;

#[cfg(target_os = "linux")]
fn ensure_tls_ready() {
    // rustls needs a process-level crypto provider before the first
    // handshake - it has no default by design. ring, over aws-lc-rs, because
    // it builds with plain cc and needs no cmake on the CI runner. Idempotent:
    // installing twice is an error we ignore on purpose.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(target_os = "macos")]
fn ensure_tls_ready() {}

// ureq treats every 4xx/5xx as an error, but the seam's contract says the
// status rides as data - the update path wants to say "HTTP 404" in its
// message and the transcribe path keys off the status itself. So status
// errors are unpacked into a normal Response here, and only genuine transport
// failures (DNS, TLS, connection refused) surface as Err.
fn finish(resp: ureq::Response) -> Result<Response, String> {
    // status() borrows, into_reader() consumes - so status comes first.
    let status = resp.status();
    let mut body = Vec::new();
    resp.into_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("reading response body failed ({e})"))?;
    Ok(Response { status, body })
}

/// A GET with an `Accept` header. Redirects are followed - the contract.
pub fn get(url: &str, accept: &str) -> Result<Response, String> {
    ensure_tls_ready();
    match ureq::get(url).set("Accept", accept).call() {
        Ok(resp) => finish(resp),
        Err(ureq::Error::Status(_, resp)) => finish(resp),
        Err(e) => Err(format!("{e}")),
    }
}

/// A POST with a raw body. `auth` is the full `Authorization` header value;
/// the caller owns the scheme prefix.
pub fn post(
    url: &str,
    auth: Option<&str>,
    content_type: &str,
    body: &[u8],
) -> Result<Response, String> {
    ensure_tls_ready();
    let mut req = ureq::post(url).set("Content-Type", content_type);
    if let Some(auth) = auth {
        req = req.set("Authorization", auth);
    }
    match req.send(body) {
        Ok(resp) => finish(resp),
        Err(ureq::Error::Status(_, resp)) => finish(resp),
        Err(e) => Err(format!("{e}")),
    }
}

/// A blocking WebSocket for the streaming path.
///
/// tungstenite's synchronous client is one object over one TCP stream, so the
/// full-duplex guarantee is approximated the way the trait docs prescribe: a
/// short-held mutex. The socket underneath is still full duplex; the lock
/// only serializes the library calls, and neither call holds it across a
/// network wait that the other side did not ask for.
pub struct UnixSocket(Arc<Mutex<WsRaw<MaybeTlsStream<std::net::TcpStream>>>>);

/// Open a WebSocket to `url`, sending `headers` on the handshake.
pub fn websocket(url: &str, headers: &[(&str, &str)]) -> Result<UnixSocket, String> {
    use tungstenite::client::IntoClientRequest;
    ensure_tls_ready();
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("not a websocket url ({e})"))?;
    for (name, value) in headers {
        // Owned name and value: the http types only take 'static keys, and
        // the caller's slices must not outlive this function anyway.
        let name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name}"))?;
        let value = http::HeaderValue::from_str(value)
            .map_err(|_| format!("invalid header value for {name}"))?;
        request.headers_mut().insert(name, value);
    }
    let (socket, _) = tungstenite::connect(request)
        .map_err(|e| format!("websocket connect failed ({e})"))?;
    Ok(UnixSocket(Arc::new(Mutex::new(socket))))
}

impl WebSocket for UnixSocket {
    fn send_binary(&self, data: &[u8]) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "websocket lock poisoned".to_string())?
            .send(Message::Binary(data.to_vec().into()))
            .map_err(|e| format!("websocket send failed ({e})"))
    }

    fn send_text(&self, text: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|_| "websocket lock poisoned".to_string())?
            .send(Message::Text(text.to_owned().into()))
            .map_err(|e| format!("websocket send failed ({e})"))
    }

    fn close(&self) {
        if let Ok(mut ws) = self.0.lock() {
            // A close frame asks the server to shut down; the reply arrives on
            // whichever thread reads next. We do not wait for it - the caller
            // means "stop now".
            let _ = ws.send(Message::Close(None));
            let _ = ws.flush();
        }
    }

    fn read(&self, timeout_ms: u32) -> Result<Option<Vec<u8>>, String> {
        let _ = timeout_ms; // see the note below
        let mut ws = self
            .0
            .lock()
            .map_err(|_| "websocket lock poisoned".to_string())?;
        loop {
            match ws.read() {
                Ok(Message::Binary(data)) => return Ok(Some(data.to_vec())),
                Ok(Message::Text(text)) => return Ok(Some(text.as_str().as_bytes().to_vec())),
                Ok(Message::Close(_)) => return Ok(None),
                // Protocol-level pings must be answered for the server to keep
                // the connection; the pong goes out on the next write, and
                // neither frame carries transcript payload.
                Ok(Message::Ping(_)) => {
                    let _ = ws.send(Message::Pong(Vec::new().into()));
                }
                Ok(Message::Pong(_)) => {}
                // The enum is non-exhaustive across versions (raw frames,
                // future additions); anything that is not payload or a ping
                // carries nothing the transcript loop needs.
                Ok(_) => {}
                Err(e) => return Err(format!("websocket read failed ({e})")),
            }
        }
    }
}

// A note on `read`'s ignored timeout, matching the Windows backend's stated
// behaviour: the streaming loop is built around a blocking read - the reader
// thread simply waits until the provider sends the next final or closes, and
// cancellation unblocks it because the writer side closes the socket. Timing
// out here would surface as `Err`, which the loop treats as the end of the
// stream, cutting off the last words of a dictation. So the parameter exists
// for backends that can poll, and this one, like WinHTTP, honestly cannot.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// The Windows backend has this exact test in update.rs, because the
    /// updater lives there. The contract belongs to the transport, not the
    /// updater: release assets are served from a CDN after a 302, and a
    /// backend that stops at the redirect downloads a short HTML page instead
    /// of a binary. So the Unix backends pin it too, through the same trait a
    /// caller would use.
    #[test]
    fn a_get_follows_the_redirect_to_the_host_the_asset_lives_on() {
        let final_body = b"asset-bytes";
        let final_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let final_addr = final_listener.local_addr().unwrap();
        let final_thread = std::thread::spawn(move || {
            let (mut conn, _) = final_listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            conn.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    final_body.len()
                )
                .as_bytes(),
            )
            .unwrap();
            conn.write_all(final_body).unwrap();
        });

        let redirect_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_addr = redirect_listener.local_addr().unwrap();
        let redirect_thread = std::thread::spawn(move || {
            let (mut conn, _) = redirect_listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            conn.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{final_addr}/cdn/asset\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
        });

        let transport = crate::platform::http::native_transport_for_tests();
        let response = transport
            .get(
                &format!("http://{redirect_addr}/releases/download/v0.1.15/asset"),
                "application/octet-stream",
            )
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(response.body, final_body.to_vec());
        redirect_thread.join().unwrap();
        final_thread.join().unwrap();
    }
}
