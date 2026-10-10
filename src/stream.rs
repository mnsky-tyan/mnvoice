// Real-time word-by-word streaming transcription for Deepgram.
// Directly types confirmed tokens into the active cursor in real-time as speech happens.
// Monotonic forward-only word streaming with zero backspaces.
//
// The transport is the platform seam, not WinHTTP directly: this module carries
// the timing-sensitive part of the product, and keeping it free of platform
// calls is what lets the Linux and macOS ports reuse it unchanged. The
// full-duplex guarantee the loop depends on is documented on the trait.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::platform::audio::SAMPLE_RATE;
use crate::platform::http::{NativeTransport, Transport, WebSocket};
use crate::platform::input;

/// Little-endian PCM bytes for a chunk of i16 samples, the wire format the
/// streaming endpoint expects (see `platform::audio::wav_bytes` for the same
/// encoding in a WAV container).
fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub struct StreamResult {
    pub transcript: String,
    pub is_final: bool,
    pub speech_final: bool,
}

/// Dependency-free frame parser. A real JSON pull would drag in a crate for
/// four fields, and the provider's payload shape is stable; the contains()
/// checks are also how frames that are not transcript results at all (metadata,
/// errors, pings) get rejected cheaply before any parsing work happens.
pub fn parse_stream_json(json: &str) -> Option<StreamResult> {
    if !json.contains("\"Results\"") && !json.contains("\"results\"") && !json.contains("\"text\"") {
        return None;
    }
    let transcript = crate::rest::parse_json_transcript(json).unwrap_or_default();
    let speech_final = json.contains("\"speech_final\":true") || json.contains("\"speech_final\": true");
    let is_final = json.contains("\"is_final\":true") || json.contains("\"is_final\": true");
    Some(StreamResult {
        transcript,
        is_final,
        speech_final,
    })
}

/// The provider's listen endpoint for this config: base URL from the config
/// (or the Deepgram default), the model and format parameters, language and
/// keywords. Pure, so the wire contract is testable without a socket.
///
/// An unparseable `BASE_URL` is an error, exactly as it is on the REST path
/// (`rest::endpoint_url`): the config is only defaulted when it is EMPTY, so a
/// present-but-broken value must fail closed rather than silently point the
/// session - and the user's API key - at Deepgram.
fn listen_url(cfg: &Config) -> Result<String, String> {
    let (host, port, secure, base_path) = crate::rest::parse_base_url(&cfg.base_url)?;

    let prefix = if !base_path.is_empty() {
        base_path
    } else {
        "/v1/listen".to_string()
    };

    // no_delay=true: release words immediately without buffering for more context (Nova-3)
    // vad_events=true: receive SpeechStarted/UtteranceEnd events from Deepgram's own VAD
    // filler_words: strip disfluencies when the provider supports it natively.
    // Providers that ignore it are still covered by the local filter below.
    let filler_words = if cfg.strip_fillers { "false" } else { "true" };
    let mut path = format!(
        "{prefix}?model={}&smart_format=true&encoding=linear16&sample_rate={SAMPLE_RATE}&channels=1&interim_results=true&endpointing=1500&no_delay=true&vad_events=true&filler_words={filler_words}",
        cfg.model
    );
    if !cfg.language.is_empty() {
        if cfg.language.eq_ignore_ascii_case("auto") {
            path.push_str("&detect_language=true");
        } else {
            path.push_str("&language=");
            path.push_str(&cfg.language);
        }
    }
    for kw in &cfg.keywords {
        let trimmed = kw.trim();
        if !trimmed.is_empty() {
            let enc = url_encode(trimmed);
            if cfg.model.contains("nova-3") {
                path.push_str("&keyterm=");
            } else {
                path.push_str("&keywords=");
            }
            path.push_str(&enc);
        }
    }

    let scheme = if secure { "wss" } else { "ws" };
    // parse_base_url returns IPv6 literals unbracketed (WinHttpConnect wants
    // them that way); a URL authority must bracket them again or the URI is
    // invalid.
    let authority = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Ok(format!("{scheme}://{authority}:{port}{path}"))
}

/// The Authorization header value for this config: Deepgram wants `Token`,
/// OpenAI-compatible endpoints want `Bearer`; whatever the user already typed
/// is passed through so either spelling works.
/// The streaming scheme is `Token`, per Deepgram; see `config::auth_value`.
fn auth_value(cfg: &Config) -> String {
    crate::config::auth_value(&cfg.api_key, "Token")
}

/// Types the words in `words[from..to]` at the cursor and appends them to the
/// running transcript, space-separated from whatever is already there. This is
/// the one place a decided word becomes typed text; the reader's branches all
/// end here so their only differences can be the range they commit.
fn commit_words(
    words: &[&str],
    from: usize,
    to: usize,
    has_typed_any: &mut bool,
    full: &Mutex<String>,
) {
    // Clamped, so a caller whose counter has run past this frame's words (a
    // shorter final transcript after a longer interim one) is a no-op rather
    // than a slice panic.
    let to = to.min(words.len());
    if from >= to {
        return;
    }
    let joined = words[from..to].join(" ");
    let mut to_type = String::new();
    if *has_typed_any {
        to_type.push(' ');
    }
    to_type.push_str(&joined);
    input::type_text(&to_type);
    *has_typed_any = true;

    let mut full = full.lock().unwrap_or_else(|e| e.into_inner());
    if !full.is_empty() {
        full.push(' ');
    }
    full.push_str(&joined);
}

/// Connects to a real-time streaming WebSocket endpoint and streams audio chunks from `rx`.
/// Types confirmed words into the active cursor as they arrive; returns the full transcript.
pub fn run_stream(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    rx: Receiver<Vec<i16>>,
) -> Result<String, String> {
    let url = listen_url(cfg)?;
    let auth_value = auth_value(cfg);
    let auth_header = ("Authorization", auth_value.as_str());

    let socket = NativeTransport.websocket(&url, &[auth_header])?;
    let ws: Arc<dyn WebSocket> = Arc::from(socket);

    let full_transcript = Arc::new(Mutex::new(String::new()));
    let reader_done = Arc::new(AtomicBool::new(false));
    // A transport failure mid-dictation is not a clean close, and the seam
    // types `read` as a Result precisely so the two can be told apart. The
    // message is kept here so the caller can report the real cause instead of
    // telling the user they said nothing.
    let read_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Capture this before the thread moves in, so the borrow cannot escape.
    let strip_fillers = cfg.strip_fillers;

    let ws_reader = Arc::clone(&ws);
    let full_transcript_clone = full_transcript.clone();
    let stop_clone = stop.clone();
    let cancelled_clone = cancelled.clone();
    let reader_done_clone = reader_done.clone();
    let read_error_clone = read_error.clone();

    let reader_thread = thread::spawn(move || {
        let mut typed_word_count = 0usize;
        let mut has_typed_any = false;
        let mut latest_uncommitted = String::new();

        while !reader_done_clone.load(Ordering::SeqCst) {
            // Cancel is honoured at the word boundary: stop reading and stop
            // typing immediately rather than waiting for speech to end.
            if cancelled_clone.load(Ordering::SeqCst) {
                break;
            }
            // A read blocks until the next frame, a close, or an error, so
            // cancel is checked before every call and honoured at the word
            // boundary; a reader parked inside `read` is released by the
            // `close()` this function issues once the session ends.
            let frame = match ws_reader.read() {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(e) => {
                    // Keep it only while the session is still running: the
                    // caller terminates the socket itself on the way out
                    // (`close()` shuts the connection down, which is what
                    // releases this parked read), so an error seen after
                    // `reader_done` was set is self-inflicted and would mark
                    // every ordinary session as lost. Likewise once `stop` is
                    // set the session is ending on purpose, so a provider that
                    // tears down without a close frame is the expected end of
                    // a finished dictation. A fault that arrives while the
                    // caller still expects frames - including one that breaks
                    // the send loop first - is the case the marker is for.
                    if !reader_done_clone.load(Ordering::SeqCst)
                        && !stop_clone.load(Ordering::SeqCst)
                    {
                        let mut slot =
                            read_error_clone.lock().unwrap_or_else(|e| e.into_inner());
                        *slot = Some(e);
                    }
                    break;
                }
            };
            if frame.is_empty() {
                break;
            }
            let msg = String::from_utf8_lossy(&frame);
            if let Some(res) = parse_stream_json(&msg) {
                let trimmed = res.transcript.trim();
                if !trimmed.is_empty() {
                    // Filter fillers before this frame is typed, so the word
                    // indices below stay aligned frame to frame.
                    let words: Vec<&str> = if strip_fillers {
                        trimmed
                            .split_whitespace()
                            .filter(|w| !crate::rest::is_disfluency(w))
                            .collect()
                    } else {
                        trimmed.split_whitespace().collect()
                    };

                    // Keep the filtered text for the end-of-stream flush, so
                    // the word indices used there match what was typed.
                    latest_uncommitted = words.join(" ");

                    if res.is_final {
                        // Sentence/clause finalized: type all remaining words to the end
                        commit_words(&words, typed_word_count, words.len(), &mut has_typed_any, &full_transcript_clone);
                        typed_word_count = 0; // reset for next clause
                        latest_uncommitted.clear();
                    } else {
                        // Interim results: type completed words (all except the trailing partial word)
                        if words.len() > 1 && words.len() - 1 > typed_word_count {
                            commit_words(&words, typed_word_count, words.len() - 1, &mut has_typed_any, &full_transcript_clone);
                            typed_word_count = words.len() - 1;
                        }
                    }

                    if res.speech_final {
                        stop_clone.store(true, Ordering::SeqCst);
                    }
                }
            }
        }

        // Flush any remaining words from the latest interim transcript upon stop/close.
        // Skipped entirely on cancel so a discarded session leaves nothing behind.
        if !latest_uncommitted.is_empty() && !cancelled_clone.load(Ordering::SeqCst) {
            let words: Vec<&str> = latest_uncommitted.split_whitespace().collect();
            commit_words(&words, typed_word_count, words.len(), &mut has_typed_any, &full_transcript_clone);
        }
        // The reader is out, so nothing more can arrive and the main thread's
        // wait can end with it rather than running out its clock.
        reader_done_clone.store(true, Ordering::SeqCst);
    });

    // Drain audio chunks from channel and stream to Deepgram
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(packet_i16) => {
                if ws.send_binary(&pcm_bytes(&packet_i16)).is_err() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    // Drain all remaining audio packets accumulated in rx before closing -
    // unless the session was cancelled: a cancel is a hard discard (the same
    // rule dictate_rest enforces before its upload), so the queued tail must
    // not be shipped to the provider after the user said stop.
    if !cancelled.load(Ordering::SeqCst) {
        while let Ok(packet_i16) = rx.try_recv() {
            let _ = ws.send_binary(&pcm_bytes(&packet_i16));
        }
    }

    // Signal close to Deepgram
    let _ = ws.send_text("{\"type\": \"CloseStream\"}");

    // Wait up to 1500ms for Deepgram to return the final transcription
    let wait_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < wait_deadline && !reader_done.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(20));
    }

    reader_done.store(true, Ordering::SeqCst);
    ws.close();

    let _ = reader_thread.join();

    let full_text = full_transcript.lock().unwrap_or_else(|e| e.into_inner()).trim().to_string();
    let read_error = read_error.lock().unwrap_or_else(|e| e.into_inner()).clone();

    // The configured trailing space is a side effect of a session that
    // produced words, and it must not depend on which way the session ended:
    // a lost connection after words were typed would otherwise be the one
    // path that commits text without it.
    if crate::rest::trailing_space_due(cfg.trailing_space, &full_text)
        && !cancelled.load(Ordering::SeqCst)
    {
        input::type_text(" ");
    }

    // A read that failed with nothing typed is a real failure and is reported
    // as one. Once words have been typed the user already has their dictation,
    // so it is kept - but the failure is still stated, in the text the caller
    // shows, because a silent partial result reads as a complete one. The user
    // pressed once and got half a sentence with no sign anything went wrong.
    //
    // No log line here: `windows_app` is Windows-only and this module compiles
    // on all three targets, and the marker travels with the returned text,
    // which every caller already reports.
    if let Some(e) = read_error {
        if full_text.is_empty() {
            return Err(format!("streaming transcription failed: {e}"));
        }
        return Ok(format!("{full_text} [connection lost: {e}]"));
    }

    Ok(full_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("C++"), "C%2B%2B");
        assert_eq!(url_encode("mnvoice"), "mnvoice");
    }

    /// Delegates to the shared constructor so a new `Config` field is added in
    /// one place, not four.
    fn test_cfg() -> Config {
        crate::config::test_config()
    }

    #[test]
    fn the_listen_url_carries_the_constant_sample_rate() {
        // The wire must name SAMPLE_RATE, not a literal: this is the rate the
        // capture engine resamples to, and a mismatch transcribes as garbage
        // with no error anywhere.
        let url = listen_url(&test_cfg()).expect("the default base URL parses");
        assert!(
            url.contains(&format!("sample_rate={SAMPLE_RATE}")),
            "the wire must carry the constant rate: {url}"
        );
        assert!(
            url.starts_with("wss://api.deepgram.com:443/v1/listen?model=nova-3&"),
            "{url}"
        );
        assert!(url.contains("&keyterm=Kubernetes"), "nova-3 uses keyterm: {url}");
    }

    #[test]
    fn an_ipv6_base_url_is_rebracketed_in_the_listen_url() {
        // parse_base_url yields the bare literal, which is what WinHTTP wants,
        // but a URL authority must bracket it: the Unix transport rejects the
        // unbracketed form, so the session never connects.
        let mut cfg = test_cfg();
        cfg.base_url = "https://[::1]:8443".into();
        let url = listen_url(&cfg).expect("a bracketed IPv6 base URL parses");
        // Drive the real Unix consumer of this string - the same
        // `IntoClientRequest` the transport calls at unix_http.rs:210 - rather
        // than only matching a prefix: the fix exists because that parser
        // rejects an unbracketed authority, so the parse succeeding here IS
        // the behavior under test. The parser lives behind the Unix target
        // gate, so this arm runs where it matters (Linux/macOS) and on Windows
        // the prefix check still pins the shape.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use tungstenite::client::IntoClientRequest;
            let request = url
                .as_str()
                .into_client_request()
                .unwrap_or_else(|e| panic!("the Unix transport must accept {url}: {e}"));
            assert_eq!(request.uri().host(), Some("[::1]"));
            assert_eq!(request.uri().port_u16(), Some(8443));
        }
        assert!(
            url.starts_with("wss://[::1]:8443/v1/listen?"),
            "the authority must bracket the IPv6 literal: {url}"
        );
    }

    /// A BASE_URL that is present but unparseable must be an error, not a
    /// silent redirect to Deepgram: the config is only defaulted when it is
    /// empty, so a typo would otherwise send the audio AND the user's API key
    /// to a provider they did not ask for. REST already fails closed here.
    #[test]
    fn an_unparseable_base_url_is_refused_not_redirected_to_deepgram() {
        let mut cfg = test_cfg();
        cfg.base_url = "localhost:8000".into(); // no scheme
        let err = listen_url(&cfg).expect_err("a scheme-less base URL must not parse");
        assert!(
            err.contains("BASE_URL"),
            "the error must name the config key that is wrong: {err}"
        );
        // A well-formed custom endpoint is still honoured verbatim, which is
        // what makes the refusal above a guard rather than a restriction.
        let mut cfg = test_cfg();
        cfg.base_url = "https://stt.corp/deepgram".into();
        let url = listen_url(&cfg).expect("a well-formed custom base URL parses");
        assert!(
            url.starts_with("wss://stt.corp:443/deepgram?"),
            "a configured path is used verbatim: {url}"
        );
    }

    #[test]
    fn the_auth_value_passes_a_prefixed_scheme_through() {
        let mut cfg = test_cfg();
        cfg.api_key = "Bearer sk-x".into();
        assert_eq!(auth_value(&cfg), "Bearer sk-x");
        cfg.api_key = "raw-key".into();
        assert_eq!(auth_value(&cfg), "Token raw-key");
    }

    /// The whole point of the connection-lost work: `run_stream` is driven
    /// against a real WebSocket peer on loopback that speaks Deepgram's
    /// framing, so the string the user is shown comes from the production
    /// function and a real socket - not from a stub. `type_text` is a no-op
    /// test injector under `cfg(test)` (see `platform::input`), so this runs
    /// the real streaming loop without touching the machine's keyboard.
    mod live_provider {
        use super::*;
        use crate::platform::audio::SAMPLE_RATE;
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::sync::atomic::AtomicBool;

        /// A server-to-client WebSocket frame: server frames are never masked.
        pub fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0x80 | opcode];
            if payload.len() < 126 {
                out.push(payload.len() as u8);
            } else if payload.len() < 65_536 {
                out.push(126);
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            } else {
                out.push(127);
                out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
            out.extend_from_slice(payload);
            out
        }

        /// Finish the HTTP upgrade by hand, then run `after_handshake` with a
        /// connected peer. Returns the port the client should dial.
        pub fn serve(
            after_handshake: impl FnOnce(&mut TcpStream) + Send + 'static,
        ) -> (u16, std::thread::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                // Read the request head up to the blank line.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(0) => return,
                        Ok(_) => head.push(byte[0]),
                        Err(_) => return,
                    }
                    if head.len() > 64 * 1024 {
                        return;
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let key = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let accept = tungstenite_accept(&key);
                let reply = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
                after_handshake(&mut stream);
            });
            (port, handle)
        }

        /// `Sec-WebSocket-Accept`: base64(sha1(key + GUID)). The crate ships a
        /// SHA-1 only inside the update module's checksum helper, so the
        /// handshake digest is spelled here from the RFC 6455 magic value.
        fn tungstenite_accept(key: &str) -> String {
            const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
            sha1_base64(format!("{key}{GUID}").as_bytes())
        }

        /// Minimal SHA-1 + base64, enough for a 20-byte handshake digest.
        fn sha1_base64(data: &[u8]) -> String {
            let digest = sha1(data);
            base64(&digest)
        }

        fn sha1(data: &[u8]) -> [u8; 20] {
            let mut h: [u32; 5] = [
                0x6745_2301,
                0xEFCD_AB89,
                0x98BA_DCFE,
                0x1032_5476,
                0xC3D2_E1F0,
            ];
            let mut msg = data.to_vec();
            let bit_len = (data.len() as u64) * 8;
            msg.push(0x80);
            while msg.len() % 64 != 56 {
                msg.push(0);
            }
            msg.extend_from_slice(&bit_len.to_be_bytes());
            for block in msg.chunks(64) {
                let mut w = [0u32; 80];
                for (i, word) in block.chunks(4).enumerate() {
                    w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
                }
                for i in 16..80 {
                    w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
                }
                let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
                for (i, &wi) in w.iter().enumerate() {
                    let (f, k) = match i {
                        0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                        20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                        40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                        _ => (b ^ c ^ d, 0xCA62_C1D6),
                    };
                    let temp = a
                        .rotate_left(5)
                        .wrapping_add(f)
                        .wrapping_add(e)
                        .wrapping_add(k)
                        .wrapping_add(wi);
                    e = d;
                    d = c;
                    c = b.rotate_left(30);
                    b = a;
                    a = temp;
                }
                h[0] = h[0].wrapping_add(a);
                h[1] = h[1].wrapping_add(b);
                h[2] = h[2].wrapping_add(c);
                h[3] = h[3].wrapping_add(d);
                h[4] = h[4].wrapping_add(e);
            }
            let mut out = [0u8; 20];
            for (i, word) in h.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
            }
            out
        }

        fn base64(data: &[u8]) -> String {
            const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in data.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                out.push(TABLE[((n >> 18) & 63) as usize] as char);
                out.push(TABLE[((n >> 12) & 63) as usize] as char);
                out.push(if chunk.len() > 1 {
                    TABLE[((n >> 6) & 63) as usize] as char
                } else {
                    '='
                });
                out.push(if chunk.len() > 2 {
                    TABLE[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            out
        }

        /// One Deepgram interim/final result frame, the shape `parse_stream_json`
        /// reads: `channel.alternatives[0].transcript` plus the two booleans.
        pub fn result_frame(transcript: &str, is_final: bool, speech_final: bool) -> Vec<u8> {
            // The `Results` envelope is what `parse_stream_json` keys off before
            // it parses anything, so a metadata frame is rejected cheaply; this
            // is the same shape Deepgram sends.
            let json = format!(
                "{{\"type\":\"Results\",\"channel\":{{\"alternatives\":[{{\"transcript\":\"{transcript}\"}}]}},\"is_final\":{is_final},\"speech_final\":{speech_final}}}"
            );
            frame(0x1, json.as_bytes())
        }

        /// Build the config that points the real streaming loop at `port`.
        pub fn cfg_for(port: u16) -> Config {
            let mut cfg = test_cfg();
            cfg.protocol = crate::config::Protocol::Streaming;
            cfg.base_url = format!("ws://127.0.0.1:{port}");
            cfg.keywords.clear();
            cfg
        }

        /// Feed `packets` of silence into the channel and return the real
        /// `run_stream` result. `stop` is set by the reader itself when it sees
        /// `speech_final`, so the audio channel only has to stay open briefly.
        pub fn drive(cfg: &Config, packets: usize) -> Result<String, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
            let n = SAMPLE_RATE as usize / 10; // 100 ms of silence per packet
            std::thread::spawn(move || {
                for _ in 0..packets {
                    if tx.send(vec![0i16; n]).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let stop = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            run_stream(cfg, &stop, &cancelled, rx)
        }
    }

    /// The intent's headline behaviour: a stream that dies after words were
    /// typed must return those words with the lost-connection marker, not a
    /// bare success that reads as a complete dictation.
    #[test]
    fn a_fault_after_words_were_typed_is_returned_with_the_connection_lost_marker() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            // Say a sentence, finalise it so the words are committed, then drop
            // the TCP connection with no WebSocket close frame: the shape of a
            // provider that dies mid-dictation.
            let _ = stream.write_all(&live_provider::result_frame("hello world", true, false));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(300));
            // Returning from the closure lets the server thread end, which
            // closes the socket the client is parked on.
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let text = out.expect("words were typed, so the result must be Ok");
        assert!(
            text.starts_with("hello world"),
            "the typed words must survive: {text}"
        );
        assert!(
            text.contains("[connection lost:"),
            "a fault after words must be marked, not silent: {text}"
        );
    }

    /// The adversarial counterpart: an orderly close frame must NOT be
    /// reported as a lost connection. Round 5's `draining` flag suppressed
    /// this, round 6 narrowed it to `stop`, and round 7 (the change under
    /// validation) scopes it to the graceful stop; this asserts the ordinary
    /// end stays quiet.
    #[test]
    fn an_orderly_close_frame_after_words_is_not_reported_as_a_lost_connection() {
        use std::io::Write as _;
        let (port, server) = live_provider::serve(|stream| {
            let _ = stream.write_all(&live_provider::result_frame("all good", true, true));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(200));
            let _ = stream.write_all(&live_provider::frame(0x8, &[]));
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(300));
            // Returning from the closure lets the server thread end, which
            // closes the socket the client is parked on.
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let text = out.expect("an orderly session is Ok");
        assert_eq!(text, "all good", "a clean close must not be marked: {text}");
    }

    /// The other half of the intent: when nothing was typed, the fault must be
    /// an error the caller reports, not the old "no speech detected".
    #[test]
    fn a_fault_before_any_words_is_an_error_not_a_silent_empty_result() {
        let (port, server) = live_provider::serve(|_stream| {
            std::thread::sleep(Duration::from_millis(200));
        });
        let cfg = live_provider::cfg_for(port);
        let out = live_provider::drive(&cfg, 40);
        let _ = server.join();

        let err = out.expect_err("nothing typed and the peer died is a failure");
        assert!(
            err.contains("streaming transcription failed"),
            "the error must say the stream failed: {err}"
        );
    }
}
