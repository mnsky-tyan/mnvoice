// The Windows transport: WinHTTP, unchanged from what mnvoice has always used.
//
// This is a move, not a rewrite. The three request shapes the app makes - a GET
// for the release feed and asset, a POST for the REST transcription fallback,
// and a WebSocket for streaming - were previously spread across `update.rs`,
// `rest.rs` and `stream.rs`, each opening its own session and closing its own
// handles. Collecting them here means the redirect behaviour that the updater
// depends on is stated once, in one type, instead of being an emergent property
// of three separate functions.
//
// WinHTTP follows redirects implicitly - `WinHttpOpenRequest` without
// `WINHTTP_DISABLE_REDIRECTS` will chase a 302 itself. That is load-bearing:
// GitHub serves release assets from a CDN via redirect, so a client that stopped
// at the 302 would download an HTML page and stage it as an executable. The
// trait documents the contract; the test in `update.rs` pins it.

use crate::platform::http::{Response, Transport, WebSocket, MAX_RESPONSE_BYTES, USER_AGENT};
use crate::rest::parse_base_url;
use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

// The windows crate does not export these two, and every WinHTTP client needs
// the same pair: query-the-status-line, as a number.
const WINHTTP_QUERY_STATUS: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

/// Build a NUL-terminated UTF-16 buffer for the wide-string Win32 APIs.
/// Shared with the tray app, which needs the same conversion for its own
/// window and tooltip text.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Borrow a wide buffer as the pointer Win32 wants. Kept separate from `wide`
/// so the temporary in `wptr(&wide(x))` lives long enough to be used.
fn wptr(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// WinHTTP-backed transport. Stateless: every call opens and closes its own
/// session, which costs little next to a network round trip and means a failed
/// request cannot poison the next one.
pub struct WinHttpTransport;

/// A session, connection and request handle that travel together.
///
/// WinHTTP handles are children of their parents: the request belongs to the
/// connection, the connection to the session. Dropping them in any other
/// order - or leaking one on an early return - leaks the parents, and a
/// session kept alive by a leaked child holds sockets for the life of the
/// process. Drop does the teardown in the one correct order, so no call site
/// has to remember it.
struct RequestHandles {
    request: Option<*mut std::ffi::c_void>,
    connect: Option<*mut std::ffi::c_void>,
    session: Option<*mut std::ffi::c_void>,
}

impl RequestHandles {
    /// A guard that owns just the session so far; the later handles are
    /// added as they are opened.
    fn with_session(session: *mut std::ffi::c_void) -> Self {
        Self {
            request: None,
            connect: None,
            session: Some(session),
        }
    }

    /// The request handle for the WinHTTP calls that take one.
    fn request(&self) -> *mut std::ffi::c_void {
        self.request.expect("request handle still held")
    }

    /// Close the request while keeping the parents - the transfer point for
    /// a WebSocket, whose socket takes over from the request.
    fn close_request(&mut self) {
        if let Some(h) = self.request.take() {
            unsafe {
                let _ = WinHttpCloseHandle(h);
            }
        }
    }

    /// Hand the parents to a WebSocket, which owns them from here on.
    fn take_parents(&mut self) -> (*mut std::ffi::c_void, *mut std::ffi::c_void) {
        let connect = self.connect.take().expect("connect handle still held");
        let session = self.session.take().expect("session handle still held");
        (connect, session)
    }
}

impl Drop for RequestHandles {
    fn drop(&mut self) {
        unsafe {
            // Children before parents, the same order WinHttpSocket's Drop
            // keeps for a live socket.
            if let Some(h) = self.request.take() {
                let _ = WinHttpCloseHandle(h);
            }
            if let Some(h) = self.connect.take() {
                let _ = WinHttpCloseHandle(h);
            }
            if let Some(h) = self.session.take() {
                let _ = WinHttpCloseHandle(h);
            }
        }
    }
}

impl WinHttpTransport {
    /// Open a session, connect, and issue a request. The handles come back
    /// in a `RequestHandles` guard whose Drop closes them in the one correct
    /// order, so every early return below needs no teardown of its own.
    unsafe fn open(
        method: &str,
        url: &str,
        timeout_ms: (i32, i32, i32),
    ) -> Result<(RequestHandles, bool), String> {
        let (host, port, secure, path) = parse_base_url(url)?;

        let session = WinHttpOpen(
            w!("mnvoice"),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            return Err("cannot create HTTP session".into());
        }
        let mut handles = RequestHandles::with_session(session);
        if let Err(e) = WinHttpSetTimeouts(
            session,
            timeout_ms.0,
            timeout_ms.1,
            timeout_ms.2,
            timeout_ms.2,
        ) {
            return Err(format!("set timeouts ({e})"));
        }

        let host_w = wide(&host);
        let connect = WinHttpConnect(session, wptr(&host_w), port, 0);
        if connect.is_null() {
            return Err(format!("cannot connect to {host}"));
        }
        handles.connect = Some(connect);

        let method_w = wide(method);
        let path_w = wide(&path);
        // dwFlags deliberately 0: WinHTTP disables redirects only when
        // WINHTTP_DISABLE_REDIRECTS is passed here, and GitHub answers release
        // asset URLs with a 302 to its CDN. Not passing the flag IS the
        // decision to follow it; the loopback test in update.rs pins it.
        let request = WinHttpOpenRequest(
            connect,
            wptr(&method_w),
            wptr(&path_w),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure {
                WINHTTP_FLAG_SECURE
            } else {
                WINHTTP_OPEN_REQUEST_FLAGS(0)
            },
        );
        if request.is_null() {
            return Err("cannot create HTTP request".into());
        }
        handles.request = Some(request);

        Ok((handles, secure))
    }

    /// Pull the status code off an open response. Only readable while the
    /// response is still open, so it must happen before the handles close.
    unsafe fn status_of(request: *mut std::ffi::c_void) -> u16 {
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        let _ = WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            &mut index,
        );
        status as u16
    }

    /// Drain the body of an open response.
    unsafe fn read_body(request: *mut std::ffi::c_void) -> Vec<u8> {
        let mut body = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        // Capped like the unix backend so the two keep one contract: a
        // misbehaving server cannot stream an unbounded body into a process
        // that is meant to stay responsive.
        let cap = MAX_RESPONSE_BYTES as usize;
        loop {
            if body.len() >= cap {
                break;
            }
            let mut read = 0u32;
            let ok = WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                chunk.len() as u32,
                &mut read,
            );
            if ok.is_err() || read == 0 {
                break;
            }
            let room = cap - body.len();
            body.extend_from_slice(&chunk[..(read as usize).min(room)]);
        }
        body
    }
}

impl Transport for WinHttpTransport {
    fn get(&self, url: &str, accept: &str) -> Result<Response, String> {
        unsafe {
            // The feed is small; 15s to connect and 45s to read is generous
            // without letting a wedged server hold the check forever.
            let (handles, _) = Self::open("GET", url, (0, 15_000, 45_000))?;
            let request = handles.request();

            let headers = wide(&format!("Accept: {accept}\r\nUser-Agent: {USER_AGENT}\r\n"));
            let result = (|| {
                // Headers go on before the send; anything added afterwards
                // never reaches the wire. Trailing CRLF trimmed because
                // WinHttpAddRequestHeaders takes a length, not a terminator.
                WinHttpAddRequestHeaders(
                    request,
                    &headers[..headers.len() - 1],
                    WINHTTP_ADDREQ_FLAG_ADD,
                )?;
                WinHttpSendRequest(request, None, None, 0, 0, 0)?;
                WinHttpReceiveResponse(request, std::ptr::null_mut())?;
                let status = Self::status_of(request);
                let body = Self::read_body(request);
                Ok::<Response, windows::core::Error>(Response { status, body })
            })();

            // The guard closes request, connection and session, in that
            // order, whatever the result was.
            drop(handles);
            result.map_err(|e| format!("request failed ({e})"))
        }
    }

    fn post(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        body: &[u8],
    ) -> Result<Response, String> {
        unsafe {
            // Uploads carry audio, so the read timeout is longer than the
            // feed's: a slow provider transcoding a long clip is not an error.
            let (handles, _) = Self::open("POST", url, (0, 10_000, 60_000))?;
            let request = handles.request();

            let auth_line = auth
                .map(|v| format!("Authorization: {v}\r\n"))
                .unwrap_or_default();
            let headers = wide(&format!(
                "{auth_line}Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                body.len()
            ));
            let result = (|| {
                WinHttpAddRequestHeaders(
                    request,
                    &headers[..headers.len() - 1],
                    WINHTTP_ADDREQ_FLAG_ADD,
                )?;
                WinHttpSendRequest(
                    request,
                    None,
                    Some(body.as_ptr() as *const std::ffi::c_void),
                    body.len() as u32,
                    body.len() as u32,
                    0,
                )?;
                WinHttpReceiveResponse(request, std::ptr::null_mut())?;
                let status = Self::status_of(request);
                let resp_body = Self::read_body(request);
                Ok::<Response, windows::core::Error>(Response {
                    status,
                    body: resp_body,
                })
            })();

            // The guard closes request, connection and session, in that
            // order, whatever the result was.
            drop(handles);
            result.map_err(|e| format!("request failed ({e})"))
        }
    }

    fn websocket(&self, url: &str, headers: &[(&str, &str)]) -> Result<Box<dyn WebSocket>, String> {
        // WINHTTP_ADDREQ_FLAG_COOKIE, named rather than written as the bare
        // literal 0x2000_0000 so the header flags this file passes read the
        // same way as the WINHTTP_ADDREQ_FLAG_ADD used by get/post above.
        const ADD_REQ_FLAG_COOKIE: u32 = 0x2000_0000;
        unsafe {
            // A streaming session is held open for the length of a dictation,
            // so there is no overall timeout and the socket carries no receive
            // timeout: `read` blocks until a frame, a close, or a transport
            // error, the contract the trait documents.
            let (mut handles, _) = Self::open("GET", url, (0, 10_000, 0))?;
            let request = handles.request();

            let opt_ok =
                WinHttpSetOption(Some(request), WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, None);
            if opt_ok.is_err() {
                return Err("cannot request WebSocket upgrade".into());
            }

            let mut header_text = String::new();
            for (k, v) in headers {
                header_text.push_str(&format!("{k}: {v}\r\n"));
            }
            // WinHTTP requires the Upgrade/Connection pair to be present before
            // the socket completes; the caller supplies auth headers only.
            if !header_text.is_empty() {
                let headers_w = wide(&header_text);
                let _ = WinHttpAddRequestHeaders(
                    request,
                    &headers_w[..headers_w.len() - 1],
                    ADD_REQ_FLAG_COOKIE,
                );
            }

            // Every failure from here to the upgrade returns through the
            // guard's Drop, which closes all three in the right order.
            if let Err(e) = WinHttpSendRequest(request, None, None, 0, 0, 0) {
                return Err(format!("WebSocket request failed ({e})"));
            }
            if let Err(e) = WinHttpReceiveResponse(request, std::ptr::null_mut()) {
                return Err(format!("WebSocket response failed ({e})"));
            }

            let status = Self::status_of(request);
            if status != 101 {
                return Err(format!("WebSocket handshake rejected with HTTP {status}"));
            }

            let ws = WinHttpWebSocketCompleteUpgrade(request, 0);
            // The socket takes over from the request: close the request, hand
            // the parents to the socket, and the guard closes nothing left.
            handles.close_request();
            if ws.is_null() {
                return Err("WebSocket upgrade failed".into());
            }
            let (connect, session) = handles.take_parents();

            Ok(Box::new(WinHttpSocket {
                ws,
                connect,
                session,
            }))
        }
    }
}

/// A live WinHTTP WebSocket, owning the handles it was upgraded from.
///
/// WinHTTP's socket handle is a child of the request's connection and session,
/// so all three travel together and are closed together in `Drop`. Dropping
/// them out of order leaks the parents.
///
/// The handle itself is just an integer to WinHTTP, which is what makes the
/// full-duplex promise above free here: a send and a receive on the same
/// handle from two threads are independent calls, exactly as the pre-seam
/// code did when it passed the raw pointer into a reader thread.
struct WinHttpSocket {
    ws: *mut std::ffi::c_void,
    connect: *mut std::ffi::c_void,
    session: *mut std::ffi::c_void,
}

// SAFETY: WinHTTP socket handles are usable concurrently for send and
// receive, and none of the three handles are mutated through this type - the
// only mutation is in Drop, which by definition has exclusive access. The
// streaming loop relies on both halves: reader thread reads while the main
// thread sends audio.
unsafe impl Send for WinHttpSocket {}
unsafe impl Sync for WinHttpSocket {}

impl WebSocket for WinHttpSocket {
    fn send_binary(&self, data: &[u8]) -> Result<(), String> {
        unsafe {
            let res = WinHttpWebSocketSend(
                self.ws,
                WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
                Some(data),
            );
            if res != 0 {
                return Err(format!("ws send (error {res})"));
            }
            Ok(())
        }
    }

    fn send_text(&self, text: &str) -> Result<(), String> {
        unsafe {
            let res = WinHttpWebSocketSend(
                self.ws,
                WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
                Some(text.as_bytes()),
            );
            if res != 0 {
                return Err(format!("ws send (error {res})"));
            }
            Ok(())
        }
    }

    fn close(&self) {
        unsafe {
            let _ = WinHttpWebSocketClose(
                self.ws,
                WINHTTP_WEB_SOCKET_SUCCESS_CLOSE_STATUS.0 as u16,
                None,
                0,
            );
        }
    }

    /// Read one frame.
    ///
    /// WinHTTP has no "read with timeout" call and the socket is opened with
    /// no receive timeout, so the receive blocks until a frame arrives, the
    /// peer closes, or the transport fails - the contract the trait
    /// documents. Any nonzero result ends the read: from the streaming loop's
    /// point of view a session that stopped delivering frames is over.
    fn read(&self) -> Result<Option<Vec<u8>>, String> {
        // Once a WinHTTP request is upgraded to a socket, its receive timeout
        // is fixed at what the session was configured with, and the pre-seam
        // behaviour this preserves is a blocking read ended by a frame or a
        // close. The streaming loop re-checks its flags before every call.
        unsafe {
            let mut buffer = vec![0u8; 64 * 1024];
            let mut read = 0u32;
            let mut buf_type = WINHTTP_WEB_SOCKET_BUFFER_TYPE::default();
            let result = WinHttpWebSocketReceive(
                self.ws,
                buffer.as_mut_ptr() as *mut std::ffi::c_void,
                buffer.len() as u32,
                &mut read,
                &mut buf_type,
            );
            // WinHTTP reports through a raw error code; zero means a frame
            // arrived. A zero-length read is the close frame, which is the end
            // of the stream, not an empty poll. Both are reported the same way
            // the streaming loop always treated them: stop.
            if result != 0 || read == 0 {
                return Ok(None);
            }
            buffer.truncate(read as usize);
            Ok(Some(buffer))
        }
    }
}

/// Types text by synthesizing keystrokes through paste.rs's event stream.
///
/// The injection contract lives on the trait; this is the Windows way of
/// honouring it. paste.rs owns the mechanics - one unicode event per
/// character with a 2 ms gap so no target window's message queue drops
/// characters - and stays the single source of that behaviour.
pub struct SendInputInjector;

/// The process-wide instance, registered as the global injector at startup by
/// the Windows app shell.
pub static SEND_INPUT_INJECTOR: SendInputInjector = SendInputInjector;

impl crate::platform::input::Injector for SendInputInjector {
    fn type_text(&self, text: &str) -> Result<(), String> {
        crate::paste::type_text(text)
    }
}

impl Drop for WinHttpSocket {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.ws);
            let _ = WinHttpCloseHandle(self.connect);
            let _ = WinHttpCloseHandle(self.session);
        }
    }
}
