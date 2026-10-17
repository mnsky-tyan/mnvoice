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

use crate::platform::http::{
    Response, Transport, WebSocket, MAX_TRANSCRIPT_BYTES, MAX_WS_MESSAGE_BYTES, USER_AGENT,
};
use crate::rest::parse_base_url;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::Networking::WinHttp::*;

// The windows crate does not export these two, and every WinHTTP client needs
// the same pair: query-the-status-line, as a number.
const WINHTTP_QUERY_STATUS: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

/// The send timeout requested for a streaming WebSocket, in the milliseconds
/// WinHTTP takes.
///
/// This is a value handed to `WinHttpSetTimeouts`, not a bound WinHTTP enforces
/// on a WebSocket send: `WinHttpWebSocketSend` ignores it, so a Windows send to
/// a peer that stopped reading blocks anyway. The bound that actually holds on
/// a stalled peer is the Unix backend's `WRITE_TIMEOUT`; on Windows there is no
/// equivalent, and `websocket` records the measurement that established it.
///
/// Kept as a plain constant rather than a `Duration` because
/// `WinHttpSetTimeouts` counts milliseconds and nothing here needs the
/// arithmetic.
const WEBSOCKET_WRITE_TIMEOUT_MS: i32 = 2_000;

/// The four timeouts `WinHttpSetTimeouts` takes: resolve, connect, send,
/// receive. Named per request shape because the same four numbers are the
/// Unix backend's `REST_*`/`DOWNLOAD_*` constants seen from this side, and a
/// bare tuple let one platform's timeout change without the other noticing.
///
/// `HTTP_REST_TIMEOUTS` mirrors `unix_http::REST_CONNECT_TIMEOUT`/`REST_IO_TIMEOUT`
/// (10s connect, 60s both directions: uploads carry audio). `HTTP_DOWNLOAD_TIMEOUTS`
/// mirrors `DOWNLOAD_CONNECT_TIMEOUT`/`DOWNLOAD_IO_TIMEOUT` (15s/45s for the
/// release feed). `WEBSOCKET_TIMEOUTS` shares the REST connect bound and carries
/// the send bound above; its receive bound is 0 - unlimited - because a streaming
/// session is held open for the length of a dictation and `read` blocks until a
/// frame, a close or a transport error, the contract the trait documents. A
/// change to either platform's numbers should land on both; the names are where
/// to look.
const HTTP_REST_TIMEOUTS: (i32, i32, i32, i32) = (0, 10_000, 60_000, 60_000);
const HTTP_DOWNLOAD_TIMEOUTS: (i32, i32, i32, i32) = (0, 15_000, 45_000, 45_000);
const WEBSOCKET_TIMEOUTS: (i32, i32, i32, i32) = (0, 10_000, WEBSOCKET_WRITE_TIMEOUT_MS, 0);

/// The close handshake bound handed to `WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT`.
///
/// Deliberately not shared with the send bound: that one is recorded as
/// ignored by `WinHttpWebSocketSend`, this one has no measurement either
/// way, and folding them into one name would make one claim bleed into
/// the other.
const WEBSOCKET_CLOSE_TIMEOUT_MS: i32 = 2_000;

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
    ///
    /// `timeout_ms` is `(resolve, connect, send, receive)` in milliseconds,
    /// with 0 meaning no timeout.
    ///
    /// Returns the handle guard alone. It used to return `(handles, secure)`,
    /// and all three callers destructured the flag straight into `_`: whether
    /// the connection is TLS decides the `dwFlags` argument inside this
    /// function and nothing downstream asks again, so the second half of the
    /// tuple was a value no consumer could use.
    unsafe fn open(
        method: &str,
        url: &str,
        timeout_ms: (i32, i32, i32, i32),
    ) -> Result<RequestHandles, String> {
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
            timeout_ms.3,
        ) {
            return Err(format!("set timeouts ({e})"));
        }

        let host_w = wide(&host);
        let connect = WinHttpConnect(session, wptr(&host_w), port, 0);
        if connect.is_null() {
            return Err(format!("cannot connect to {host}"));
        }
        handles.connect = Some(connect);

        // One user agent for every request this transport makes. The session
        // opened above is named `w!("mnvoice")`, and WinHTTP would put that on
        // the wire for a request that adds no User-Agent header of its own -
        // which is what made the identity depend on the method: only the GET
        // spelled out `USER_AGENT`, so a transcription POST identified as
        // "mnvoice" while the update feed identified as "mnvoice-update". The
        // trait's doc (see `platform::http::USER_AGENT`) is that the two
        // backends send one string, so it is added here, where every method
        // passes, and no caller spells it again.
        let agent_w = wide(&format!("User-Agent: {USER_AGENT}\r\n"));
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

        // Added before the handle is returned, so it applies to every caller
        // (GET, POST, WebSocket upgrade).
        //
        // The binding passes `slice.len()` as `dwHeadersLength`, so the slice
        // handed over must not count `wide`'s appended UTF-16 NUL as header
        // text. What the slice drops is THAT terminator, not the header's own
        // trailing CRLF: the string still ends in CRLF when it goes to
        // WinHTTP, which is the line ending the API expects per header line.
        // (An earlier comment here said the CRLF was dropped, which was never
        // what the arithmetic did.)
        if let Err(e) = WinHttpAddRequestHeaders(
            request,
            &agent_w[..agent_w.len() - 1],
            WINHTTP_ADDREQ_FLAG_ADD,
        ) {
            return Err(format!("set user agent ({e})"));
        }

        Ok(handles)
    }

    /// Pull the status code off an open response. Only readable while the
    /// response is still open, so it must happen before the handles close.
    ///
    /// The query result is propagated, not discarded: a failure used to leave
    /// `status` at 0 and hand back `Ok(status)`, so a response whose headers
    /// could not be read looked like an HTTP 0. Every one of the three call
    /// sites still rejects a bogus 0 - the WebSocket upgrade compares 101
    /// inline, and GET and POST carry the status out to callers that demand
    /// 200 - so the failure surfaced as an error rather than a false success.
    /// What was lost was the Win32 reason behind it, and a status of 0 is a
    /// strange thing to report as a status code.
    unsafe fn status_of(request: *mut std::ffi::c_void) -> windows::core::Result<u16> {
        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            &mut index,
        )?;
        Ok(status as u16)
    }

    /// Drain the body of an open response, up to `max_bytes`.
    ///
    /// Capped like the unix backend so the two keep one contract: a misbehaving
    /// server cannot stream an unbounded body into a process that is meant to
    /// stay responsive. The bound is passed in rather than fixed here because
    /// this one function serves both a JSON transcript and a release
    /// executable.
    ///
    /// A failed read is an error, not the end of the body. `read == 0` is what
    /// means "no more bytes"; `WinHttpReadData` returning an error used to
    /// break the same way, so a connection that died mid-body was handed back
    /// as a short body with a 200 status and no trace of the failure. The unix
    /// backend propagates that case as `Err` (`unix_http::finish`), and the
    /// REST path - which types its result straight into the user's focused
    /// window - was the hazard: a truncated transcript would have looked like
    /// a complete one. The updater is safe either way because its checksum
    /// gate re-reads the staged file.
    unsafe fn read_body(
        request: *mut std::ffi::c_void,
        max_bytes: u64,
    ) -> windows::core::Result<Vec<u8>> {
        let mut body = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        let cap = max_bytes as usize;
        loop {
            if body.len() >= cap {
                break;
            }
            let mut read = 0u32;
            WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                chunk.len() as u32,
                &mut read,
            )?;
            if read == 0 {
                break;
            }
            let room = cap - body.len();
            body.extend_from_slice(&chunk[..(read as usize).min(room)]);
        }
        Ok(body)
    }
}

impl Transport for WinHttpTransport {
    fn get(&self, url: &str, accept: &str, max_bytes: u64) -> Result<Response, String> {
        unsafe {
            // The feed is small; 15s to connect and 45s to read is generous
            // without letting a wedged server hold the check forever.
            let handles = Self::open("GET", url, HTTP_DOWNLOAD_TIMEOUTS)?;
            let request = handles.request();

            let headers = wide(&format!("Accept: {accept}\r\n"));
            let result = (|| {
                // Headers go on before the send; anything added afterwards
                // never reaches the wire. As in `open`, the element removed is
                // `wide`'s appended NUL (the length argument counts header
                // characters), and the CRLF that ends the header line stays.
                WinHttpAddRequestHeaders(
                    request,
                    &headers[..headers.len() - 1],
                    WINHTTP_ADDREQ_FLAG_ADD,
                )?;
                WinHttpSendRequest(request, None, None, 0, 0, 0)?;
                WinHttpReceiveResponse(request, std::ptr::null_mut())?;
                let status = Self::status_of(request)?;
                let body = Self::read_body(request, max_bytes)?;
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
            let handles = Self::open("POST", url, HTTP_REST_TIMEOUTS)?;
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
                let status = Self::status_of(request)?;
                let resp_body = Self::read_body(request, MAX_TRANSCRIPT_BYTES)?;
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
        unsafe {
            // A streaming session is held open for the length of a dictation,
            // so there is no overall timeout and the socket carries no receive
            // timeout: `read` blocks until a frame, a close, or a transport
            // error, the contract the trait documents.
            //
            // The send timeout is asked for at request creation rather than after
            // it: a request inherits the session's timeouts when it is opened, so
            // a bound set after the open may never reach the request this socket
            // is upgraded from. The tuple asks for resolve 0, connect 10s, send
            // 2s, receive 0.
            //
            // It does not bound the send, and this is measured rather than
            // assumed. Against a real loopback peer that completes the WinHTTP
            // handshake and then stops reading, `WinHttpWebSocketSend` stayed
            // blocked past 60s with this value at 2s, past 120s with it at 100ms,
            // and past 90s with WINHTTP_OPTION_SEND_TIMEOUT set on the request
            // handle as well. WinHTTP exposes no option that bounds a WebSocket
            // send - the only WebSocket-specific timeout is
            // WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT, which bounds the close
            // handshake - so the post-session drain in `stream.rs` still parks
            // with the close frame and the reader join behind it on this
            // platform. The effective bound on a stalled peer is the Unix
            // backend's `WRITE_TIMEOUT`; that asymmetry is the reason this is
            // written down rather than left to the constant's name.
            let mut handles = Self::open("GET", url, WEBSOCKET_TIMEOUTS)?;
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
            //
            // The result is checked, exactly as the get and post paths check the
            // same call: a header WinHTTP refuses (an API key carrying CR/LF, say
            // - nothing validates that spelling upstream) used to be dropped
            // silently, and the session then failed as a generic provider 401
            // or as an unauthenticated socket that broke on every later send.
            if !header_text.is_empty() {
                let headers_w = wide(&header_text);
                if let Err(e) = WinHttpAddRequestHeaders(
                    request,
                    &headers_w[..headers_w.len() - 1],
                    WINHTTP_ADDREQ_FLAG_ADD,
                ) {
                    return Err(format!("WebSocket header rejected ({e})"));
                }
            }

            // Every failure from here to the upgrade returns through the
            // guard's Drop, which closes all three in the right order.
            if let Err(e) = WinHttpSendRequest(request, None, None, 0, 0, 0) {
                return Err(format!("WebSocket request failed ({e})"));
            }
            if let Err(e) = WinHttpReceiveResponse(request, std::ptr::null_mut()) {
                return Err(format!("WebSocket response failed ({e})"));
            }

            let status = Self::status_of(request)
                .map_err(|e| format!("WebSocket status unreadable ({e})"))?;
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
            // Bounded here, before the handle is boxed, so the close handshake
            // `close()` performs cannot outlive the session on a peer that never
            // answers it.
            //
            // Requested, not proven: unlike the send bound, which this file
            // measured as ignored by `WinHttpWebSocketSend`, no measurement
            // against a peer that never answers a close frame exists yet - so
            // the option is set and the claim that the handshake is actually
            // bounded stays open.
            //
            // A refusal is logged rather than propagated: killing the session
            // over a missing bound trades a working (if unbounded) close for no
            // dictation at all, and the default teardown is what every earlier
            // release shipped with.
            let socket = WinHttpSocket {
                ws,
                connect,
                session,
            };
            if let Err(e) = socket.set_close_timeout(WEBSOCKET_CLOSE_TIMEOUT_MS) {
                crate::windows_app::log(&format!(
                    "close timeout not set, teardown stays on WinHTTP's default ({e})"
                ));
            }
            Ok(Box::new(socket))
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

impl WinHttpSocket {
    /// Bound the close handshake this socket performs when `close` runs.
    ///
    /// `WinHttpWebSocketClose` is not a fire-and-forget: it exchanges a close
    /// frame with the peer, and the only thing that bounds that exchange is
    /// `WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT`. Left unset it takes WinHTTP's
    /// default rather than this caller's, which is what let `close` block on a
    /// peer that never answers - the Windows half of a drift the Unix backend
    /// does not have, because it terminates the socket outright
    /// (`shutdown(Both)`) instead of negotiating.
    ///
    /// Set on the socket rather than the request: the option belongs to the
    /// WebSocket handle, and by the time this runs the request has already been
    /// closed and handed over.
    ///
    /// A private inherent method, not a trait method, because the trait is the
    /// seam both backends share and only this one has the concept.
    ///
    /// Returns whether the option took, so the caller can log a refusal. A
    /// refused option is not something `close` can route around - there is no
    /// second bound to fall back to - but it is the only signal that the
    /// handshake is still on WinHTTP's default, and the caller records it
    /// rather than dropping it. It is deliberately not propagated: the default
    /// teardown still works, and failing the whole session over a missing bound
    /// would turn a slower close into no dictation at all.
    ///
    /// The bound is *requested* here, not proven. This file already recorded
    /// that `WINHTTP_OPTION_SEND_TIMEOUT` and `WinHttpSetTimeouts` are set and
    /// ignored for a WebSocket send, so nothing here may be asserted as
    /// honoured without a measurement against a peer that never answers a close
    /// frame - which this change does not contain.
    fn set_close_timeout(&self, ms: i32) -> Result<(), String> {
        unsafe {
            WinHttpSetOption(
                Some(self.ws),
                WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT,
                Some(ms.to_ne_bytes().as_slice()),
            )
        }
        .map_err(|e| format!("set close timeout ({e})"))
    }
}

/// The error for a `WinHttpWebSocketSend` that did not succeed.
///
/// One owner for both send paths, in the house pattern the Unix backend uses
/// for the same job (`unix_http::socket_setup`): the wording is what a user
/// reads and what a log grep matches, so two copies would be two places to
/// keep in step. The phrasing deliberately matches the Unix backend's
/// "websocket send failed (...)" - the two transports land in the same log
/// line, and a reader should not have to know which one produced a failure.
/// The WinHTTP error number is carried through because it is the only clue to
/// a stalled-send diagnosis.
fn send_failed(res: u32) -> String {
    format!("websocket send failed (winhttp {res})")
}

impl WebSocket for WinHttpSocket {
    fn send_binary(&self, data: &[u8]) -> Result<(), String> {
        unsafe {
            let res = WinHttpWebSocketSend(
                self.ws,
                WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
                Some(data),
            );
            if res != 0 {
                return Err(send_failed(res));
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
                return Err(send_failed(res));
            }
            Ok(())
        }
    }

    /// Close the connection.
    ///
    /// Diverges from the trait's `close` ("send a close frame and shut down
    /// without waiting for the peer") in the one way WinHTTP cannot avoid. The
    /// trait expects a backend that cannot honour a promise to say so at its
    /// own implementation, which is what this note is; the same instruction
    /// appears on `read` ("a backend that cannot block this way must say so in
    /// its implementation"), and this method diverges in exactly the way that
    /// sentence anticipates.
    ///
    /// `WinHttpWebSocketClose` is a negotiated close: it sends a close frame
    /// and waits for the peer's, bounded only by
    /// `WINHTTP_OPTION_WEB_SOCKET_CLOSE_TIMEOUT` - which `set_close_timeout`
    /// requests and cannot prove is honoured. Two consequences the trait's
    /// promise does not have on Unix:
    ///
    /// 1. This can wait on the peer. The Unix backend shuts the socket down
    ///    outright (`shutdown(Both)`), which is what makes its `close`
    ///    immediate and what releases a reader parked in its `read`.
    /// 2. It does not release a parked `WinHttpWebSocketReceive`, so the
    ///    reader thread's `join` in `stream.rs` can outlive this call until the
    ///    peer sends something or the transport fails. `WinHttpWebSocketShutdown`
    ///    is not the escape hatch: it closes the SEND channel and explicitly
    ///    leaves the receive channel open. WinHTTP has no socket abort that
    ///    would end a pending receive, so there is nothing to route around this
    ///    with.
    ///
    /// The consequence is bounded, not silent: the session has already been
    /// reported to the user, and the reader exits as soon as the peer speaks
    /// or the transport notices the teardown. Closing it properly needs a
    /// design decision (poll in `read`, close the session handle from here and
    /// accept the receive's error, or drop the promise from the trait), which
    /// is why this documents the gap rather than pretending to close it.
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

    /// Read one whole message.
    ///
    /// WinHTTP has no "read with timeout" call and the socket is opened with
    /// no receive timeout, so the receive blocks until a frame arrives, the
    /// peer closes, or the transport fails - the contract the trait
    /// documents. The two ends are kept apart, because the streaming loop
    /// reports a lost connection only when the read says so: a nonzero result
    /// is a transport failure and becomes an `Err`, while a close frame
    /// arrives as `WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE` and becomes `Ok(None)`.
    /// Collapsing the two would make a provider disconnect indistinguishable
    /// from an orderly end, which is exactly the confusion the trait's
    /// `Result` exists to prevent.
    ///
    /// `peBufferType` is the API's own report of what came back, and this
    /// reads it: the trait's consumer treats every `read` as one complete JSON
    /// message (`stream.rs` parses the whole frame), so a fragment returned as
    /// if it were whole would fail that parse and silently drop words - the
    /// Unix backend reassembles through tungstenite and would not. Three
    /// shapes therefore need telling apart, and the byte count alone cannot do
    /// it: a fragment type accumulates until the message ends, `ERROR_MORE_DATA`
    /// (a message longer than the 64 KiB buffer) continues into the next call
    /// rather than failing the session, and the close type ends the stream
    /// even though a legal empty message is also a zero-length read.
    fn read(&self) -> Result<Option<Vec<u8>>, String> {
        // Once a WinHTTP request is upgraded to a socket, its receive timeout
        // is fixed at what the session was configured with, and the pre-seam
        // behaviour this preserves is a blocking read ended by a frame or a
        // close. The streaming loop re-checks its flags before every call.
        unsafe {
            let mut message: Vec<u8> = Vec::new();
            // One receive buffer for the whole message. It used to be rebuilt
            // - allocated and zeroed, 64 KiB at a time - inside the loop, so a
            // message arriving in N fragments paid for N buffers to hold the
            // same bytes. `read_body` above shows the intended shape: a buffer
            // hoisted outside the loop and reused. Re-allocating per fragment
            // is the same defect class as the per-packet silent-packet zeros
            // this repo already removed (commit 909c836), and a fragmented
            // transcript frame is exactly the ordinary case on a provider that
            // streams word-by-word.
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                let mut buf_type = WINHTTP_WEB_SOCKET_BUFFER_TYPE::default();
                let result = WinHttpWebSocketReceive(
                    self.ws,
                    buffer.as_mut_ptr() as *mut std::ffi::c_void,
                    buffer.len() as u32,
                    &mut read,
                    &mut buf_type,
                );
                let more_data = result == ERROR_MORE_DATA.0;
                if result != 0 && !more_data {
                    return Err(format!("websocket read failed (error {result})"));
                }
                if buf_type == WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE {
                    return Ok(None);
                }
                let is_fragment = buf_type == WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE
                    || buf_type == WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE;
                if read > 0 {
                    message.extend_from_slice(&buffer[..read as usize]);
                }
                // The accumulation is bounded. tungstenite refuses a message
                // over its configured cap on Unix; without this check the same
                // peer behaviour on Windows grew `message` without limit, one
                // 64 KiB fragment at a time, in a process whose whole job is to
                // stay responsive. The error ends the session like any other
                // transport failure, and `stream.rs` reports it.
                if message.len() > MAX_WS_MESSAGE_BYTES {
                    return Err(format!(
                        "websocket message exceeded {MAX_WS_MESSAGE_BYTES} bytes"
                    ));
                }
                // `more_data` and a fragment type both mean this message is
                // not finished; anything else is a whole message.
                if !more_data && !is_fragment {
                    return Ok(Some(message));
                }
            }
        }
    }
}

/// Types text by synthesizing keystrokes through paste.rs's event stream.
///
/// The injection contract lives on the trait; this is the Windows way of
/// honouring it. paste.rs owns the mechanics - one unicode event per
/// character with a 2 ms gap so no target window's message queue drops
/// characters - and stays the single source of that behaviour.
// Both are named by `platform_injector`, whose body is compiled on every build
// (under tests `global` returns the no-op injector, so it is never *called*
// there). A reference from that live body is enough to keep them reachable -
// the shape verified on the Unix builds, where the same arrangement cleared
// `default_injector`, its injector struct and `KEY_GAP` with one attribute at
// the root. The suppression therefore lives once, on `platform_injector`.
//
// This used to read "compiled out under tests", which was true while the
// function carried `#[cfg(not(test))]` and is what orphaned the whole injector
// chain into a warning per backend in a test build.
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
