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
// therefore the platform's own verifier and keychain trust. Each side needs
// one line of setup the other does not: Linux installs rustls' crypto
// provider, which rustls refuses to pick a default for, and macOS installs
// the native-tls connector on every agent - ureq's default connector is
// rustls whenever its `tls` feature is on and nothing at all when it is off,
// and the `native-tls` feature supplies only the adapter, never the default.
//
// Like the Windows transport, every call is stateless: open, complete, close.
// A failed request cannot poison the next one.

use crate::platform::http::{Response, Transport, WebSocket};
use std::io::Read as _;
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

// tungstenite re-exports the http types its handshake needs.
use tungstenite::http;
use tungstenite::WebSocket as WsRaw;

/// Per-phase bounds on a REST request, mirroring the WinHTTP session this
/// replaces: `WinHttpSetTimeouts` bounds connect, send and receive separately,
/// and a max-length clip has to clear the upload and the provider's transcode
/// as two budgets rather than sharing one clock. One overall timeout covered
/// both, so a slow uplink plus a slow transcode failed where the Windows build
/// succeeded.
const REST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REST_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-phase bounds on an updater download, same shape as the Windows GET.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_IO_TIMEOUT: Duration = Duration::from_secs(45);

/// Bound on the TCP connect and on the WebSocket handshake that follows it.
/// Windows bounds the same steps; std's and ureq's defaults are either looser
/// than a dictation can wait or absent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The agent each platform starts from, before the timeout split is applied.
///
/// ureq's default TLS connector is rustls whenever its `tls` feature is on and
/// nothing at all when it is off. The macOS manifest turns `tls` off so the
/// bundled Mozilla roots cannot stand in for Security.framework, which leaves
/// the connector to be installed here.
#[cfg(target_os = "macos")]
fn agent_builder() -> Result<ureq::AgentBuilder, String> {
    let connector = Arc::new(
        ureq::native_tls::TlsConnector::new()
            .map_err(|e| format!("cannot initialise TLS ({e})"))?,
    );
    Ok(ureq::AgentBuilder::new().tls_connector(connector))
}

/// rustls is compiled in here and is ureq's own default connector, so the
/// agent needs no TLS setup of its own.
#[cfg(target_os = "linux")]
fn agent_builder() -> Result<ureq::AgentBuilder, String> {
    Ok(ureq::AgentBuilder::new())
}

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

/// One agent per request shape, so each mirrors a WinHTTP session's timeout
/// split. ureq's `timeout_connect`/`timeout_read`/`timeout_write` map onto
/// WinHTTP's connect/send/receive, and no overall timeout is set because
/// WinHTTP sets none either - an overall clock is what cut a long upload short.
/// They are built once rather than per call so the connection pool survives,
/// which is what the global agent behind `ureq::get` would have provided.
static REST_AGENT: OnceLock<Result<ureq::Agent, String>> = OnceLock::new();
static DOWNLOAD_AGENT: OnceLock<Result<ureq::Agent, String>> = OnceLock::new();

fn rest_agent() -> Result<&'static ureq::Agent, String> {
    REST_AGENT
        .get_or_init(|| {
            agent_builder().map(|builder| {
                builder
                    .timeout_connect(REST_CONNECT_TIMEOUT)
                    .timeout_read(REST_IO_TIMEOUT)
                    .timeout_write(REST_IO_TIMEOUT)
                    .build()
            })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

fn download_agent() -> Result<&'static ureq::Agent, String> {
    DOWNLOAD_AGENT
        .get_or_init(|| {
            agent_builder().map(|builder| {
                builder
                    .timeout_connect(DOWNLOAD_CONNECT_TIMEOUT)
                    .timeout_read(DOWNLOAD_IO_TIMEOUT)
                    .timeout_write(DOWNLOAD_IO_TIMEOUT)
                    .build()
            })
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// A GET with an `Accept` header. Redirects are followed - the contract.
pub fn get(url: &str, accept: &str) -> Result<Response, String> {
    ensure_tls_ready();
    match download_agent()?.get(url).set("Accept", accept).call() {
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
    let mut req = rest_agent()?.post(url).set("Content-Type", content_type);
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
/// network wait that the other side did not ask for - the socket carries a
/// short read timeout, so a quiet provider releases the lock between polls.
pub struct UnixSocket {
    socket: Arc<Mutex<WsRaw<MaybeTlsStream<TcpStream>>>>,
    /// The handle `close` terminates the connection through. The stream itself
    /// moves into the TLS wrapper, so shutting the socket down needs a second
    /// descriptor on it.
    shutdown: TcpStream,
}

/// How long the reader waits for a frame before releasing the lock, and how
/// long it stands off before trying again. Both are the same number because
/// they exist for the same reason: the contract on the trait forbids holding
/// the lock across a wait for a frame that has not arrived, and a send on the
/// main thread must never queue behind the provider's silence. One
/// millisecond is the smallest value that is not zero - a zero read timeout
/// means "block forever" to the socket, not "return immediately".
const READ_POLL_MS: u64 = 1;

/// Open a WebSocket to `url`, sending `headers` on the handshake.
///
/// The TCP connection is made here rather than by `tungstenite::connect` so
/// that timeouts can be put on the socket: `connect` leaves the connect and
/// the handshake unbounded, and a host that resolves but never answers would
/// hold the CLI forever.
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

    let uri = request.uri();
    let host = uri
        .host()
        .ok_or_else(|| format!("websocket url has no host ({url})"))?
        .to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("ws") | Some("http") => 80,
        _ => 443,
    });

    let stream = connect_with_timeout(&host, port, CONNECT_TIMEOUT)?;
    // `connect_timeout` completes the connect through a non-blocking socket,
    // so the mode is set back explicitly rather than assumed.
    stream
        .set_nonblocking(false)
        .map_err(|e| format!("websocket socket setup failed ({e})"))?;
    // The handshake runs under its own bound; it is replaced by the much
    // shorter poll interval once the socket exists.
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("websocket socket setup failed ({e})"))?;
    // tungstenite's own `connect` sets this; the streaming loop types words as
    // they arrive, so a delayed small frame is a visible stutter.
    stream
        .set_nodelay(true)
        .map_err(|e| format!("websocket socket setup failed ({e})"))?;
    // A second handle on the same socket: the stream itself moves into the TLS
    // wrapper, and a duplicated descriptor shares the socket's options.
    let options = stream
        .try_clone()
        .map_err(|e| format!("websocket socket setup failed ({e})"))?;

    let (socket, _) = tungstenite::client_tls_with_config(request, stream, None, None)
        .map_err(|e| format!("websocket connect failed ({e})"))?;
    options
        .set_read_timeout(Some(Duration::from_millis(READ_POLL_MS)))
        .map_err(|e| format!("websocket socket setup failed ({e})"))?;

    Ok(UnixSocket {
        socket: Arc::new(Mutex::new(socket)),
        shutdown: options,
    })
}

/// Connects to `host:port`, trying every address it resolves to with `timeout`
/// applied to each. `TcpStream::connect` has no bound of its own, and a host
/// that resolves but never completes a handshake would otherwise hold the
/// caller for as long as the operating system cares to wait.
fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host} ({e})"))?
        .collect();
    let mut last_err = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(match last_err {
        Some(e) => format!("websocket connect failed ({e})"),
        None => format!("cannot resolve {host}"),
    })
}

/// The transport for both Unix backends.
///
/// Linux and macOS differ only in their TLS stack, and that difference is
/// already decided by Cargo.toml's per-target features plus the setup each
/// side needs, so one type serves both. What genuinely differs per platform -
/// the audio device and the injector - stays in `linux_impl` and `macos_impl`.
pub struct UnixTransport;

impl Transport for UnixTransport {
    fn get(&self, url: &str, accept: &str) -> Result<Response, String> {
        get(url, accept)
    }

    fn post(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> Result<Response, String> {
        post(url, auth, content_type, body)
    }

    fn websocket(&self, url: &str, headers: &[(&str, &str)]) -> Result<Box<dyn WebSocket>, String> {
        Ok(Box::new(websocket(url, headers)?))
    }
}

impl WebSocket for UnixSocket {
    fn send_binary(&self, data: &[u8]) -> Result<(), String> {
        self.socket
            .lock()
            .map_err(|_| "websocket lock poisoned".to_string())?
            .send(Message::Binary(data.to_vec().into()))
            .map_err(|e| format!("websocket send failed ({e})"))
    }

    fn send_text(&self, text: &str) -> Result<(), String> {
        self.socket
            .lock()
            .map_err(|_| "websocket lock poisoned".to_string())?
            .send(Message::Text(text.to_owned().into()))
            .map_err(|e| format!("websocket send failed ({e})"))
    }

    fn close(&self) {
        if let Ok(mut ws) = self.socket.lock() {
            // A close frame asks the server to shut down; the reply arrives on
            // whichever thread reads next. We do not wait for it - the caller
            // means "stop now".
            let _ = ws.send(Message::Close(None));
            let _ = ws.flush();
        }
        // The frame alone does not end the connection, and a peer that has
        // stopped answering never replies to one. Terminating the socket is
        // what lets a reader parked in `read` return, so the caller's join
        // over its reader thread stays bounded.
        let _ = self.shutdown.shutdown(Shutdown::Both);
    }

    fn read(&self, timeout_ms: u32) -> Result<Option<Vec<u8>>, String> {
        let _ = timeout_ms; // see the note below
        loop {
            let mut ws = self
                .socket
                .lock()
                .map_err(|_| "websocket lock poisoned".to_string())?;
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
                // The socket's read timeout turns "the provider is quiet" into
                // a poll miss rather than a wait: the lock is dropped before
                // the next attempt, so a send on another thread never queues
                // behind a frame that has not arrived yet.
                Err(tungstenite::Error::Io(ref e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // Standing off the lock for as long as the reader was
                    // willing to wait for a frame is what makes the bound real:
                    // a thread that unlocks and immediately locks again wins
                    // that race every time, so a sender parked in `lock()`
                    // would wait for however long the reader cared to keep
                    // polling.
                    drop(ws);
                    thread::sleep(Duration::from_millis(READ_POLL_MS));
                }
                Err(e) => return Err(format!("websocket read failed ({e})")),
            }
        }
    }
}

// A note on `read`'s ignored timeout, matching the Windows backend's stated
// behaviour: the streaming loop is built around a blocking read - the reader
// thread simply waits until the provider sends the next final or closes, and
// cancellation unblocks it because the writer side closes the socket. The
// trait has no spelling for "nothing arrived this tick": `None` and `Err` are
// both read by the loop as the end of the stream, so honouring the deadline
// here would cut off the last words of a dictation. The socket's short read
// timeout therefore bounds only how long the mutex can be held, which is what
// keeps a send on the main thread moving while the provider is quiet.

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    // `Read` comes in through `super::*`; only `Write` is new here.
    use std::io::Write as _;

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

        let transport: Box<dyn Transport> = Box::new(UnixTransport);
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

    /// `close` is the last thing the streaming loop does before it joins its
    /// reader thread, and the trait promises it shuts the connection down. A
    /// peer that completes the handshake and then goes silent leaves that
    /// reader parked in `read`, so unless the socket itself is terminated the
    /// join never returns - the same thing WinHTTP's close does.
    #[test]
    fn close_unblocks_a_reader_waiting_on_a_silent_peer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let len = conn.read(&mut buf).unwrap();
            let key = String::from_utf8_lossy(&buf[..len])
                .lines()
                .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap()
                .trim()
                .to_string();
            let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
            conn.write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
            // The provider has answered the handshake and now says nothing
            // more: the connection stays open until the client goes away.
            let mut sink = [0u8; 512];
            while let Ok(n) = conn.read(&mut sink) {
                if n == 0 {
                    break;
                }
            }
        });

        let socket = Arc::new(websocket(&format!("ws://{addr}/"), &[]).unwrap());
        let reader_socket = Arc::clone(&socket);
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = outcome_tx.send(reader_socket.read(1000));
        });

        // Let the reader park inside `read` before asking the socket to stop.
        thread::sleep(Duration::from_millis(200));
        socket.close();

        match outcome_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(outcome) => assert!(
                !matches!(outcome, Ok(Some(_))),
                "a peer that never sent a frame must not produce one"
            ),
            Err(_) => panic!("close left a reader waiting for a frame from a silent peer"),
        }
        reader.join().unwrap();
        server.join().unwrap();
    }
}
